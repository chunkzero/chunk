//! Serves dashboard assets and a separately authenticated management API.

use std::{future::Future, io, net::SocketAddr, path::PathBuf};

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower_http::services::{ServeDir, ServeFile};

/// Configuration supplied by the backend, never exposed to the browser.
pub struct Config {
    pub bind: SocketAddr,
    pub dashboard_dir: PathBuf,
    pub token: String,
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    functions: bool,
    reconciliation: bool,
    asset_uploads: bool,
}

/// Serve the dashboard until shutdown is requested.
///
/// # Errors
/// Returns an error for invalid configuration, missing assets, or listener failure.
pub async fn run(config: Config, shutdown: impl Future<Output = ()> + Send + 'static) -> io::Result<()> {
    if !config.dashboard_dir.join("index.html").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "dashboard index.html missing; run just dashboard-build first",
        ));
    }
    let app = router(&config)?;
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await
}

fn router(config: &Config) -> io::Result<Router> {
    if config.token.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "management token is empty"));
    }
    let token = HeaderValue::from_str(&format!("Bearer {}", config.token))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid management token"))?;
    let token: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let api = Router::new()
        .route("/status", get(status))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(token, authorize));
    let assets = ServeDir::new(config.dashboard_dir.join("assets"));
    let dashboard =
        ServeDir::new(&config.dashboard_dir).fallback(ServeFile::new(config.dashboard_dir.join("index.html")));

    Ok(Router::new()
        .nest("/api", api)
        .route("/api", get(|| async { StatusCode::NOT_FOUND }))
        .nest_service("/assets", assets)
        .fallback_service(dashboard))
}

async fn status() -> Json<Status> {
    Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        functions: false,
        reconciliation: false,
        asset_uploads: false,
    })
}

async fn authorize(State(expected): State<[u8; 32]>, request: Request, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .map_or(&[][..], HeaderValue::as_bytes);
    let supplied: [u8; 32] = Sha256::digest(supplied).into();
    if !bool::from(supplied.ct_eq(&expected)) {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(header::WWW_AUTHENTICATE, "Bearer")
            .body(axum::body::Body::empty())
            .expect("fixed response is valid");
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use tower::ServiceExt;

    #[tokio::test]
    async fn management_requires_credentials_and_returns_real_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(&Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            dashboard_dir: dir.path().into(),
            token: "test-token".into(),
        })
        .unwrap();
        for token in [
            None,
            Some("Bearer wrong"),
            Some("Bearer test-tokem"),
            Some("Bearer test-token-extra"),
        ] {
            let mut request = Request::builder().uri("/api/status");
            if let Some(token) = token {
                request = request.header(header::AUTHORIZATION, token);
            }
            let response = app.clone().oneshot(request.body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/status")
                    .header(header::AUTHORIZATION, "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(
            String::from_utf8(body.to_vec())
                .unwrap()
                .contains("\"reconciliation\":false")
        );
    }

    #[tokio::test]
    async fn browser_routes_use_shell_but_missing_api_and_assets_do_not() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "dashboard shell").unwrap();
        let app = router(&Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            dashboard_dir: dir.path().into(),
            token: "test-token".into(),
        })
        .unwrap();
        for (path, expected) in [
            ("/sessions", StatusCode::OK),
            ("/api/missing", StatusCode::NOT_FOUND),
            ("/assets/missing.js", StatusCode::NOT_FOUND),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::AUTHORIZATION, "Bearer test-token")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
    }
}
