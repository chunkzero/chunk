//! The readiness endpoint: plain HTTP/1.1 on an optional address, for an operator's failover check.

use std::{convert::Infallible, time::Duration};

use http_body_util::Full;
use hyper::{Method, Request, Response, StatusCode, body::Bytes, service::service_fn};
use hyper_util::rt::TokioIo;
use tokio::{net::TcpListener, task::JoinSet};

use crate::routes::Routes;

/// How long a connection may stay open.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

/// Answers `GET /ready` on `listener` for as long as it runs; dropping it closes the open connections.
pub(crate) async fn serve(listener: TcpListener, routes: Routes) {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let routes = routes.clone();
                    let service = service_fn(move |request: Request<_>| {
                        let response = respond(&request, &routes);
                        async move { Ok::<_, Infallible>(response) }
                    });
                    let connection = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service);
                    connections.spawn(async move { _ = tokio::time::timeout(CONNECTION_TIMEOUT, connection).await });
                }
                Err(error) => {
                    tracing::warn!(%error, "health accept failed; retrying");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
}

fn respond<B>(request: &Request<B>, routes: &Routes) -> Response<Full<Bytes>> {
    let (status, body) = match (request.method(), request.uri().path()) {
        (&Method::GET, "/ready") if routes.loaded() => (StatusCode::OK, "ready\n"),
        (&Method::GET, "/ready") => (StatusCode::SERVICE_UNAVAILABLE, "routes not loaded\n"),
        _ => (StatusCode::NOT_FOUND, "not found\n"),
    };
    let mut response = Response::new(Full::from(body));
    *response.status_mut() = status;
    response
}
