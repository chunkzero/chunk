//! Serves dashboard assets and a separately authenticated management API.

mod logs;
mod system;

use std::{
    future::Future,
    io,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
    time::Instant,
};

use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::Response,
    routing::get,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower_http::services::{ServeDir, ServeFile};

pub use logs::{Entry, LogLayer, Logs};
pub use system::{Machine, Sample};

/// Configuration supplied by the backend, never exposed to the browser.
pub struct Config {
    pub bind: SocketAddr,
    pub dashboard_dir: PathBuf,
    pub token: String,
    /// Applications to list until discovery through a deploy pipeline exists.
    pub projects: Vec<Project>,
    pub projects_file: Option<PathBuf>,
    pub backend: Backend,
    pub logs: Arc<Logs>,
}

/// Facts about the running edge that the dashboard reports.
pub struct Backend {
    pub minecraft_bind: SocketAddr,
    pub motd: String,
    pub max_connections: usize,
    pub connections: Arc<AtomicUsize>,
    pub started: Instant,
}

#[derive(Clone)]
struct AppState {
    projects: Arc<Vec<Project>>,
    logs: Arc<Logs>,
    info: Arc<Info>,
    machine: Arc<Machine>,
}

struct Info {
    backend: Backend,
    management_bind: SocketAddr,
    dashboard_dir: PathBuf,
    projects_file: Option<PathBuf>,
}

#[derive(Serialize)]
struct Status {
    version: &'static str,
    functions: bool,
    reconciliation: bool,
    asset_uploads: bool,
    uptime_seconds: u64,
    minecraft_bind: SocketAddr,
    management_bind: SocketAddr,
    motd: String,
    max_connections: usize,
    connections: usize,
    dashboard_dir: PathBuf,
    projects_file: Option<PathBuf>,
}

/// An application known to this backend, with the deployments running from it.
#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    pub deployments: Vec<Deployment>,
}

/// Where the application's code lives.
#[derive(Clone, Serialize, Deserialize)]
pub struct Source {
    pub repository: String,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub name: String,
    pub environment: Environment,
    #[serde(default)]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub deployed_at: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Environment {
    Production,
    Development,
    Preview,
}

impl Project {
    /// Parse a JSON array of projects, as written by an operator or a future deploy step.
    ///
    /// # Errors
    /// Returns an error when the document is not a valid project list.
    pub fn parse_list(json: &str) -> io::Result<Vec<Self>> {
        serde_json::from_str(json).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }
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
    let bind = config.bind;
    let app = router(config)?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app).with_graceful_shutdown(shutdown).await
}

fn router(config: Config) -> io::Result<Router> {
    let state = AppState {
        projects: Arc::new(config.projects),
        logs: config.logs,
        info: Arc::new(Info {
            backend: config.backend,
            management_bind: config.bind,
            dashboard_dir: config.dashboard_dir.clone(),
            projects_file: config.projects_file,
        }),
        machine: Arc::default(),
    };
    if config.token.trim().is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "management token is empty"));
    }
    let token = HeaderValue::from_str(&format!("Bearer {}", config.token))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid management token"))?;
    let token: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let api = Router::new()
        .route("/status", get(status))
        .route("/projects", get(list_projects))
        .route("/logs", get(list_logs))
        .route("/system", get(system))
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(middleware::from_fn_with_state(token, authorize))
        .with_state(state);
    let assets = ServeDir::new(config.dashboard_dir.join("assets"));
    let dashboard =
        ServeDir::new(&config.dashboard_dir).fallback(ServeFile::new(config.dashboard_dir.join("index.html")));

    Ok(Router::new()
        .nest("/api", api)
        .route("/api", get(|| async { StatusCode::NOT_FOUND }))
        .nest_service("/assets", assets)
        .fallback_service(dashboard))
}

async fn status(State(state): State<AppState>) -> Json<Status> {
    let info = &state.info;
    Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        functions: false,
        reconciliation: false,
        asset_uploads: false,
        uptime_seconds: info.backend.started.elapsed().as_secs(),
        minecraft_bind: info.backend.minecraft_bind,
        management_bind: info.management_bind,
        motd: info.backend.motd.clone(),
        max_connections: info.backend.max_connections,
        connections: info.backend.connections.load(Ordering::Relaxed),
        dashboard_dir: info.dashboard_dir.clone(),
        projects_file: info.projects_file.clone(),
    })
}

#[derive(serde::Deserialize)]
struct After {
    #[serde(default)]
    after: u64,
}

async fn list_logs(State(state): State<AppState>, Query(query): Query<After>) -> Json<Vec<Entry>> {
    Json(state.logs.since(query.after))
}

async fn system(State(state): State<AppState>) -> Json<Sample> {
    Json(state.machine.sample())
}

async fn list_projects(State(state): State<AppState>) -> Json<Vec<Project>> {
    Json(state.projects.as_ref().clone())
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

    fn test_config(dir: &std::path::Path) -> Config {
        Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            dashboard_dir: dir.into(),
            token: "test-token".into(),
            projects: Vec::new(),
            projects_file: None,
            backend: Backend {
                minecraft_bind: "127.0.0.1:25565".parse().unwrap(),
                motd: "test".into(),
                max_connections: 8,
                connections: Arc::default(),
                started: Instant::now(),
            },
            logs: Arc::default(),
        }
    }

    #[tokio::test]
    async fn management_requires_credentials_and_returns_real_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let app = router(test_config(dir.path())).unwrap();
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
        let app = router(test_config(dir.path())).unwrap();
        for (path, expected) in [
            ("/sessions", StatusCode::OK),
            ("/api/projects", StatusCode::OK),
            ("/api/logs?after=0", StatusCode::OK),
            ("/api/system", StatusCode::OK),
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

    #[test]
    fn example_projects_file_parses() {
        let projects = Project::parse_list(include_str!("../../../examples/projects.json")).unwrap();
        assert_eq!(projects[0].id, "chunk");
    }

    #[test]
    fn logs_are_bounded_and_resumable() {
        let logs = Logs::default();
        for index in 0..1005 {
            logs.record("INFO", "test", format!("event {index}"));
        }
        let all = logs.since(0);
        assert_eq!(all.len(), 1000);
        assert_eq!(all[0].seq, 6);
        assert_eq!(logs.since(1003).len(), 2);
    }
}
