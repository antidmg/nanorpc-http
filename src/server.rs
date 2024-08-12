use std::{convert::Infallible, net::SocketAddr};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use nanorpc::{JrpcRequest, RpcService};
use tokio::net::TcpListener;

/// An HTTP-based nanorpc server.
pub struct HttpRpcServer {
    listener: TcpListener,
}

impl HttpRpcServer {
    /// Creates a new HTTP server listening at the given address. Routing is currently not supported.
    pub async fn bind(addr: SocketAddr) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// Runs the server, processing requests with the given [RpcService], blocking indefinitely until a fatal failure happens. This must be run for the server to make any progress!
    pub async fn run(&self, service: impl RpcService + Clone + 'static) -> std::io::Result<()> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let io = TokioIo::new(stream);
            let service_clone = service.clone();

            tokio::spawn(async move {
                let service_fn = service_fn(move |req: Request<Incoming>| {
                    let service = service_clone.clone();
                    async move {
                        match handle_request(req, service).await {
                            Ok(response) => Ok::<_, Infallible>(response),
                            Err(err) => Ok(Response::builder()
                                .status(StatusCode::INTERNAL_SERVER_ERROR)
                                .body(Full::new(Bytes::from(err.to_string())))
                                .unwrap()),
                        }
                    }
                });

                let connection = hyper::server::conn::http1::Builder::new()
                    .keep_alive(true)
                    .serve_connection(io, service_fn);

                if let Err(err) = connection.await {
                    eprintln!("Error serving connection: {:?}", err);
                }
            });
        }
    }
}

async fn handle_request(
    req: Request<Incoming>,
    service: impl RpcService,
) -> anyhow::Result<Response<Full<Bytes>>> {
    if req.method() != hyper::Method::POST {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::from("Only POST method is allowed")))
            .unwrap());
    }

    let body = req.into_body().collect().await?.to_bytes();
    let jrpc_req: JrpcRequest = serde_json::from_slice(&body)?;
    let jrpc_response = service.respond_raw(jrpc_req).await;
    let response_body = serde_json::to_vec(&jrpc_response)?;

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(response_body)))
        .unwrap())
}
