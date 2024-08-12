use std::{
    net::SocketAddr,
    sync::RwLock,
    time::{Duration, Instant},
};

use async_socks5::AddrKind;
use async_trait::async_trait;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{client::conn::http1::SendRequest, Request};
use hyper_util::rt::TokioIo;
use nanorpc::{JrpcRequest, JrpcResponse, RpcTransport};
use std::io::Result as IoResult;
use tokio::sync::Mutex;
use tokio::{net::TcpStream, time::timeout};

type Conn = SendRequest<Full<Bytes>>;

/// An HTTP-based [RpcTransport] for nanorpc.
pub struct HttpRpcTransport {
    remote: String,
    proxy: Proxy,
    pool: Mutex<Vec<(Conn, Instant)>>,
    idle_timeout: Duration,
    req_timeout: RwLock<Duration>,
}

#[derive(Clone)]
pub enum Proxy {
    Direct,
    Socks5(SocketAddr),
}

impl HttpRpcTransport {
    /// Create a new HttpRpcTransport that goes to the given socket address over unencrypted HTTP/1.1. A default timeout is applied.
    ///
    /// Currently, custom paths, HTTPS, etc are not supported.
    pub fn new(remote: String, proxy: Proxy) -> Self {
        Self {
            remote,
            proxy,
            pool: Mutex::new(Vec::with_capacity(64)),
            idle_timeout: Duration::from_secs(60),
            req_timeout: Duration::from_secs(30).into(),
        }
    }

    /// Sets the timeout for this transport. If `None`, disables any timeout handling.
    pub fn set_timeout(&self, timeout: Option<Duration>) {
        *self.req_timeout.write().unwrap() = timeout.unwrap_or(Duration::MAX);
    }

    /// Gets the timeout for this transport.
    pub fn timeout(&self) -> Option<Duration> {
        let to = *self.req_timeout.read().unwrap();
        if to == Duration::MAX {
            None
        } else {
            Some(to)
        }
    }

    /// Opens a new connection to the remote.
    async fn open_conn(&self) -> IoResult<Conn> {
        let mut pool = self.pool.lock().await;
        while let Some((conn, expiry)) = pool.pop() {
            if expiry.elapsed() < self.idle_timeout {
                return Ok(conn);
            }
        }

        let stream = match &self.proxy {
            Proxy::Direct => TcpStream::connect(self.remote.clone()).await?,
            Proxy::Socks5(proxy_addr) => {
                let mut tcp_stream = TcpStream::connect(proxy_addr).await?;

                // TODO: this doesn't handle normal domains with .haven, find a smarter way.
                let processed_remote: AddrKind = if self.remote.contains(".haven") {
                    let tuple = parse_haven_url(&self.remote).expect("invalid haven address");
                    AddrKind::Domain(tuple.0, tuple.1)
                } else {
                    AddrKind::Ip(
                        self.remote
                            .parse::<SocketAddr>()
                            .expect("invalid socket address"),
                    )
                };
                async_socks5::connect(&mut tcp_stream, processed_remote, None)
                    .await
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

                tcp_stream
            }
        };

        let io = TokioIo::new(stream);

        let (conn, handle) = hyper::client::conn::http1::handshake(io)
            .await
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;

        tokio::spawn(async move {
            let _ = handle.await;
        });

        Ok(conn)
    }
}

// TODO: write a FromStr if it's cleaner
pub fn parse_haven_url(url_str: &str) -> anyhow::Result<(String, u16)> {
    // Remove the scheme (http:// or https://)
    let url_without_scheme = url_str
        .split("://")
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("Invalid URL format"))?;

    // Split the remaining part into host and port
    let parts: Vec<&str> = url_without_scheme.split(':').collect();
    if parts.len() == 2 {
        let host = parts[0].to_string();
        let port: u16 = parts[1]
            .parse()
            .map_err(|_| anyhow::anyhow!("Invalid port number"))?;
        Ok((host, port))
    } else if parts.len() == 1 {
        Ok((parts[0].to_string(), 80)) // Default to port 80 if not specified
    } else {
        Err(anyhow::anyhow!("Invalid URL format"))
    }
}

#[async_trait]
impl RpcTransport for HttpRpcTransport {
    type Error = std::io::Error;

    async fn call_raw(&self, req: JrpcRequest) -> Result<JrpcResponse, Self::Error> {
        let timeout_duration = *self.req_timeout.read().unwrap();
        let result = timeout(timeout_duration, async {
            let mut conn = self.open_conn().await?;
            let response = conn
                .send_request(
                    Request::builder()
                        .method("POST")
                        .body(Full::new(serde_json::to_vec(&req)?.into()))
                        .expect("could not build request"),
                )
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?;
            let response = response
                .into_body()
                .collect()
                .await
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e))?
                .to_bytes();
            let resp = serde_json::from_slice(&response)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            let mut pool = self.pool.lock().await;
            pool.push((conn, Instant::now()));
            Ok(resp)
        })
        .await;

        match result {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "nanorpc-http request timed out",
            )),
        }
    }
}
