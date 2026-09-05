//! Serves dashboard assets and a separately authenticated management API.

mod git;
mod logs;
mod projects;
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
use serde::Serialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tower_http::services::{ServeDir, ServeFile};

pub use logs::{Entry, LogLayer, Logs};
pub use projects::{Deployment, Environment, Project, Source};
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
    projects: Arc<projects::Store>,
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

#[derive(Serialize, utoipa::ToSchema)]
struct Status {
    version: &'static str,
    project_editing: bool,
    uptime_seconds: u64,
    #[schema(value_type = String)]
    minecraft_bind: SocketAddr,
    #[schema(value_type = String)]
    management_bind: SocketAddr,
    motd: String,
    max_connections: usize,
    connections: usize,
    #[schema(value_type = String)]
    dashboard_dir: PathBuf,
    #[schema(value_type = Option<String>)]
    projects_file: Option<PathBuf>,
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
        projects: Arc::new(projects::Store::new(config.projects, config.projects_file.clone())),
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
    let api = api_router()
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

fn api_router() -> Router<AppState> {
    documented_router().split_for_parts().0
}

fn documented_router() -> utoipa_axum::router::OpenApiRouter<AppState> {
    utoipa_axum::router::OpenApiRouter::new()
        .routes(utoipa_axum::routes!(status))
        .routes(utoipa_axum::routes!(list_projects))
        .routes(utoipa_axum::routes!(projects::update_source))
        .routes(utoipa_axum::routes!(projects::add_target))
        .routes(utoipa_axum::routes!(projects::update_target, projects::remove_target))
        .routes(utoipa_axum::routes!(git::list_branches))
        .routes(utoipa_axum::routes!(list_logs))
        .routes(utoipa_axum::routes!(system))
}

/// The management contract used to generate the browser client.
#[must_use]
pub fn openapi() -> utoipa::openapi::OpenApi {
    use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityRequirement, SecurityScheme};
    let mut document = documented_router().split_for_parts().1;
    document
        .components
        .get_or_insert_default()
        .add_security_scheme("bearer", SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)));
    document.security = Some(vec![SecurityRequirement::new("bearer", Vec::<String>::new())]);
    document.servers = Some(vec![utoipa::openapi::Server::new("/api")]);
    document
}

#[utoipa::path(get, path = "/status", responses((status = 200, body = Status), (status = 401)))]
async fn status(State(state): State<AppState>) -> Json<Status> {
    let info = &state.info;
    Json(Status {
        version: env!("CARGO_PKG_VERSION"),
        project_editing: info.projects_file.is_some(),
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

#[derive(serde::Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
struct After {
    #[serde(default)]
    after: u64,
    stream: Option<String>,
}

#[utoipa::path(get, path = "/logs", params(After), responses((status = 200, body = logs::Batch), (status = 401)))]
async fn list_logs(State(state): State<AppState>, Query(query): Query<After>) -> Json<logs::Batch> {
    Json(state.logs.batch(query.after, query.stream.as_deref()))
}

#[utoipa::path(get, path = "/system", responses((status = 200, body = Sample), (status = 401), (status = 503)))]
async fn system(State(state): State<AppState>) -> Result<Json<Sample>, StatusCode> {
    tokio::task::spawn_blocking(move || Json(state.machine.sample()))
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

#[utoipa::path(get, path = "/projects", responses((status = 200, body = Vec<Project>), (status = 401), (status = 503, body = projects::ErrorBody)))]
async fn list_projects(State(state): State<AppState>) -> Result<Json<Vec<Project>>, projects::ApiError> {
    tokio::task::spawn_blocking(move || state.projects.list().map(Json))
        .await
        .map_err(|_| projects::ApiError::unavailable())?
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
    async fn management_requires_credentials_and_returns_server_status() {
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
                .contains("\"project_editing\":false")
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
            ("/api/git/branches?repository=file:///tmp/repo", StatusCode::BAD_REQUEST),
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

    async fn mutation(
        app: &Router,
        method: &str,
        path: &str,
        body: serde_json::Value,
        authenticated: bool,
    ) -> Response {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json");
        if authenticated {
            request = request.header(header::AUTHORIZATION, "Bearer test-token");
        }
        app.clone()
            .oneshot(request.body(Body::from(body.to_string())).unwrap())
            .await
            .unwrap()
    }

    fn editable_app(dir: &std::path::Path) -> (Router, PathBuf) {
        let file = dir.join("projects.json");
        std::fs::write(&file, include_str!("../../../examples/projects.json")).unwrap();
        let mut config = test_config(dir);
        config.projects = Project::parse_list(&std::fs::read_to_string(&file).unwrap()).unwrap();
        config.projects_file = Some(file.clone());
        (router(config).unwrap(), file)
    }

    #[tokio::test]
    async fn application_edits_persist_and_targets_are_not_deployments() {
        let dir = tempfile::tempdir().unwrap();
        let (app, file) = editable_app(dir.path());
        let source = serde_json::json!({"repository": "https://github.com/chunkzero/example", "branch": "release"});
        assert_eq!(
            mutation(&app, "PUT", "/api/projects/chunk/source", source.clone(), false)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            mutation(&app, "PUT", "/api/projects/chunk/source", source, true)
                .await
                .status(),
            StatusCode::OK
        );
        let target = serde_json::json!({"branch": "feature/new-world", "environment": "development"});
        assert_eq!(
            mutation(&app, "POST", "/api/projects/chunk/targets", target.clone(), false)
                .await
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            mutation(&app, "POST", "/api/projects/chunk/targets", target.clone(), true)
                .await
                .status(),
            StatusCode::CREATED
        );
        assert_eq!(
            mutation(&app, "POST", "/api/projects/chunk/targets", target.clone(), true)
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            mutation(
                &app,
                "POST",
                "/api/projects/chunk/targets",
                serde_json::json!({"branch":"bad..branch", "environment":"development"}),
                true
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        let saved = Project::parse_list(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved[0].source.as_ref().unwrap().branch.as_deref(), Some("release"));
        assert_eq!(saved[0].deployments.len(), 4);
        assert!(saved[0].deployments[0].commit.is_some());
        let new_target = saved[0].deployments.last().unwrap();
        assert_eq!(new_target.git_ref.as_deref(), Some("feature/new-world"));
        assert_eq!(new_target.name, "feature/new-world");
        assert!(new_target.commit.is_none());
        assert!(new_target.deployed_at.is_none());
        assert_eq!(saved[1].id, "lobby");
        let mut restarted = test_config(dir.path());
        restarted.projects = saved;
        restarted.projects_file = Some(file);
        let response = router(restarted)
            .unwrap()
            .oneshot(
                Request::builder()
                    .uri("/api/projects")
                    .header(header::AUTHORIZATION, "Bearer test-token")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let projects: Vec<Project> =
            serde_json::from_slice(&to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(projects[0].deployments.len(), 4);
    }

    #[tokio::test]
    async fn targets_can_be_edited_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let (app, file) = editable_app(dir.path());
        let target = serde_json::json!({"branch": "feature/new-world", "environment": "development"});
        assert_eq!(
            mutation(&app, "POST", "/api/projects/chunk/targets", target.clone(), true)
                .await
                .status(),
            StatusCode::CREATED
        );
        let saved = Project::parse_list(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let new_target = saved[0].deployments.last().unwrap();
        let path = format!("/api/projects/chunk/targets/{}", new_target.id);
        let edited = serde_json::json!({"branch": "main", "environment": "development", "name": "Staging"});
        // Moving onto the production branch and environment collides with the existing target.
        let taken = serde_json::json!({"branch": "main", "environment": "production"});
        assert_eq!(
            mutation(&app, "PUT", &path, taken, true).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            mutation(&app, "PUT", &path, edited, true).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            mutation(
                &app,
                "DELETE",
                "/api/projects/chunk/targets/missing",
                serde_json::json!(null),
                true
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let saved = Project::parse_list(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let edited = saved[0].deployments.last().unwrap();
        assert_eq!(
            (edited.name.as_str(), edited.git_ref.as_deref()),
            ("Staging", Some("main"))
        );
        assert_eq!(
            mutation(&app, "DELETE", &path, serde_json::json!(null), true)
                .await
                .status(),
            StatusCode::OK
        );
        let saved = Project::parse_list(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved[0].deployments.len(), 3);
        assert_eq!(
            mutation(&app, "POST", "/api/projects/chunk/targets", target, true)
                .await
                .status(),
            StatusCode::CREATED
        );
        let saved = Project::parse_list(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved[0].deployments.len(), 4);
    }

    #[test]
    fn example_projects_file_parses() {
        let projects = Project::parse_list(include_str!("../../../examples/projects.json")).unwrap();
        assert_eq!(projects[0].id, "chunk");
    }

    #[test]
    fn log_cursor_handles_gaps_and_restarts() {
        let logs = Logs::default();
        logs.record("INFO", "test", "first".into());
        let first = logs.batch(0, None);
        assert!(first.reset);
        assert!(!first.truncated);
        assert!(logs.batch(first.cursor, Some(&first.stream)).entries.is_empty());
        for _ in 0..1001 {
            logs.record("INFO", "test", "next".into());
        }
        let gap = logs.batch(first.cursor, Some(&first.stream));
        assert!(gap.truncated);
        assert!(!gap.reset);
        assert_eq!(gap.entries.len(), 1000);
        let restarted = Logs::default();
        restarted.record("INFO", "test", "restarted".into());
        let reset = restarted.batch(gap.cursor, Some(&gap.stream));
        assert!(reset.reset);
        assert_eq!(reset.entries[0].message, "restarted");
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
