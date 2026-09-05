//! Application configuration and branch targets persisted in the projects file.

use super::AppState;
use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Write as _},
    path::PathBuf,
    sync::Mutex,
};

/// An application known to this backend, with the deployments running from it.
#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub source: Option<Source>,
    #[serde(default)]
    #[schema(required = true)]
    pub deployments: Vec<Deployment>,
}

/// Where the application's code lives.
#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Source {
    pub repository: String,
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Clone, Serialize, Deserialize, utoipa::ToSchema)]
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
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

pub(super) struct Store {
    projects: Mutex<Vec<Project>>,
    file: Option<PathBuf>,
}

impl Store {
    pub(super) fn new(projects: Vec<Project>, file: Option<PathBuf>) -> Self {
        Self {
            projects: Mutex::new(projects),
            file,
        }
    }

    pub(super) fn list(&self) -> Result<Vec<Project>, ApiError> {
        Ok(self.projects.lock().map_err(|_| ApiError::unavailable())?.clone())
    }

    fn update(&self, id: &str, change: impl FnOnce(&mut Project) -> Result<(), ApiError>) -> Result<Project, ApiError> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "Editing is not available on this server."))?;
        let mut current = self.projects.lock().map_err(|_| ApiError::unavailable())?;
        // Read the current file so a save also preserves external configuration edits.
        let file = file.canonicalize().map_err(storage_error)?;
        let mut updated =
            Project::parse_list(&std::fs::read_to_string(&file).map_err(storage_error)?).map_err(storage_error)?;
        let project = updated
            .iter_mut()
            .find(|project| project.id == id)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Application not found."))?;
        change(project)?;
        let result = project.clone();
        let parent = file.parent().ok_or_else(ApiError::unavailable)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(storage_error)?;
        temporary
            .as_file()
            .set_permissions(std::fs::metadata(&file).map_err(storage_error)?.permissions())
            .map_err(storage_error)?;
        serde_json::to_writer_pretty(&mut temporary, &updated)
            .map_err(|error| storage_error(io::Error::other(error)))?;
        temporary.write_all(b"\n").map_err(storage_error)?;
        temporary.as_file().sync_all().map_err(storage_error)?;
        temporary.persist(&file).map_err(|error| storage_error(error.error))?;
        *current = updated;
        Ok(result)
    }
}

#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct ErrorBody {
    message: String,
}

pub(super) struct ApiError {
    status: StatusCode,
    message: &'static str,
}

impl ApiError {
    pub(super) fn new(status: StatusCode, message: &'static str) -> Self {
        Self { status, message }
    }
    pub(super) fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Application data is unavailable. Try again.",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorBody {
                message: self.message.into(),
            }),
        )
            .into_response()
    }
}

#[expect(clippy::needless_pass_by_value, reason = "Used directly with Result::map_err.")]
fn storage_error(error: io::Error) -> ApiError {
    tracing::warn!(%error, "could not save application configuration");
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Changes could not be saved. Try again.",
    )
}

#[utoipa::path(put, path = "/projects/{id}/source", params(("id" = String, Path)), request_body = Source,
    responses((status = 200, body = Project), (status = 400, body = ErrorBody), (status = 401), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub(super) async fn update_source(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(mut source): Json<Source>,
) -> Result<Json<Project>, ApiError> {
    source.repository = source.repository.trim().to_owned();
    validate_repository(&source.repository)?;
    source.branch = source
        .branch
        .map(|branch| branch.trim().to_owned())
        .filter(|branch| !branch.is_empty());
    if let Some(branch) = &source.branch {
        validate_branch(branch)?;
    }
    edit(state, id, move |project| {
        project.source = Some(source);
        Ok(())
    })
    .await
    .map(Json)
}

/// The branch and environment a target records, with an optional display name.
#[derive(Deserialize, utoipa::ToSchema)]
pub(super) struct TargetSpec {
    branch: String,
    environment: Environment,
    /// Defaults to the branch name.
    #[serde(default)]
    name: Option<String>,
}

impl TargetSpec {
    fn validated(mut self) -> Result<Self, ApiError> {
        self.branch = self.branch.trim().to_owned();
        validate_branch(&self.branch)?;
        self.name = self
            .name
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty());
        if self.name.as_ref().is_some_and(|name| name.len() > 100) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "Target names must be 100 characters or fewer.",
            ));
        }
        Ok(self)
    }

    /// Rejects a branch and environment pair already used by another target.
    fn ensure_unique(&self, project: &Project, except: Option<&str>) -> Result<(), ApiError> {
        if project.deployments.iter().any(|entry| {
            except != Some(entry.id.as_str())
                && entry.git_ref.as_deref() == Some(&self.branch)
                && entry.environment == self.environment
        }) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "This branch already has a target in that environment.",
            ));
        }
        Ok(())
    }
}

async fn edit(
    state: AppState,
    id: String,
    change: impl FnOnce(&mut Project) -> Result<(), ApiError> + Send + 'static,
) -> Result<Project, ApiError> {
    tokio::task::spawn_blocking(move || state.projects.update(&id, change))
        .await
        .map_err(|_| ApiError::unavailable())?
}

#[utoipa::path(post, path = "/projects/{id}/targets", params(("id" = String, Path)), request_body = TargetSpec,
    responses((status = 201, body = Project), (status = 400, body = ErrorBody), (status = 401), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub(super) async fn add_target(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(target): Json<TargetSpec>,
) -> Result<(StatusCode, Json<Project>), ApiError> {
    let target = target.validated()?;
    let project = edit(state, id, move |project| {
        if project.source.is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "Link a Git repository before adding a branch.",
            ));
        }
        target.ensure_unique(project, None)?;
        let key = serde_json::to_vec(&(&target.branch, &target.environment)).map_err(|_| ApiError::unavailable())?;
        let id = format!("branch-{:x}", Sha256::digest(key));
        if project.deployments.iter().any(|entry| entry.id == id) {
            return Err(ApiError::new(StatusCode::CONFLICT, "This target already exists."));
        }
        project.deployments.push(Deployment {
            id,
            name: target.name.unwrap_or_else(|| target.branch.clone()),
            environment: target.environment,
            git_ref: Some(target.branch),
            commit: None,
            deployed_at: None,
        });
        Ok(())
    })
    .await?;
    Ok((StatusCode::CREATED, Json(project)))
}

#[utoipa::path(put, path = "/projects/{id}/targets/{target}", params(("id" = String, Path), ("target" = String, Path)), request_body = TargetSpec,
    responses((status = 200, body = Project), (status = 400, body = ErrorBody), (status = 401), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub(super) async fn update_target(
    State(state): State<AppState>,
    Path((id, target_id)): Path<(String, String)>,
    Json(target): Json<TargetSpec>,
) -> Result<Json<Project>, ApiError> {
    let target = target.validated()?;
    edit(state, id, move |project| {
        target.ensure_unique(project, Some(&target_id))?;
        let deployment = project
            .deployments
            .iter_mut()
            .find(|entry| entry.id == target_id)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "Target not found."))?;
        deployment.name = target.name.unwrap_or_else(|| target.branch.clone());
        deployment.environment = target.environment;
        deployment.git_ref = Some(target.branch);
        Ok(())
    })
    .await
    .map(Json)
}

#[utoipa::path(delete, path = "/projects/{id}/targets/{target}", params(("id" = String, Path), ("target" = String, Path)),
    responses((status = 200, body = Project), (status = 401), (status = 404, body = ErrorBody), (status = 409, body = ErrorBody), (status = 503, body = ErrorBody)))]
pub(super) async fn remove_target(
    State(state): State<AppState>,
    Path((id, target_id)): Path<(String, String)>,
) -> Result<Json<Project>, ApiError> {
    edit(state, id, move |project| {
        let before = project.deployments.len();
        project.deployments.retain(|entry| entry.id != target_id);
        if project.deployments.len() == before {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "Target not found."));
        }
        Ok(())
    })
    .await
    .map(Json)
}

pub(super) fn validate_repository(repository: &str) -> Result<(), ApiError> {
    let url_valid = url::Url::parse(repository).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https" | "ssh")
            && url.host_str().is_some()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && (url.scheme() == "ssh" || url.username().is_empty())
            && !url.path().trim_matches('/').is_empty()
    });
    let scp_valid = repository
        .strip_prefix("git@")
        .and_then(|value| value.split_once(':'))
        .is_some_and(|(host, path)| !host.is_empty() && !host.contains('/') && !path.is_empty());
    if repository.len() > 2048 || repository.chars().any(char::is_whitespace) || !(url_valid || scp_valid) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Enter an HTTPS or SSH Git repository URL without credentials.",
        ));
    }
    Ok(())
}

#[expect(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "Git branch suffix rules are case-sensitive."
)]
fn validate_branch(branch: &str) -> Result<(), ApiError> {
    if branch.is_empty()
        || branch.len() > 255
        || branch == "@"
        || branch == "HEAD"
        || branch.starts_with('-')
        || branch.ends_with('.')
        || branch.contains("..")
        || branch.contains("@{")
        || branch
            .bytes()
            .any(|byte| byte <= b' ' || byte == 127 || b"~^:?*[\\".contains(&byte))
        || branch
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "Enter a valid Git branch name."));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_git_sources_and_branch_names() {
        for repository in [
            "https://github.com/chunkzero/chunk",
            "ssh://git@example.com/team/repo.git",
            "git@example.com:team/repo.git",
        ] {
            assert!(validate_repository(repository).is_ok(), "{repository}");
        }
        for repository in [
            "",
            "not a repository",
            "file:///tmp/repo",
            "https://user:secret@example.com/repo",
            "https://example.com",
        ] {
            assert!(validate_repository(repository).is_err(), "{repository}");
        }
        for branch in ["main", "feature/new-world", "release/v1.2"] {
            assert!(validate_branch(branch).is_ok());
        }
        for branch in [
            "",
            "HEAD",
            "-main",
            "bad..branch",
            "a.lock/b",
            ".hidden",
            "bad branch",
            "a//b",
            "main@{1}",
            "a/b.",
        ] {
            assert!(validate_branch(branch).is_err(), "{branch}");
        }
    }

    #[test]
    fn failed_save_keeps_the_last_published_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("projects.json");
        let original = include_str!("../../../examples/projects.json");
        std::fs::write(&file, original).unwrap();
        let store = Store::new(Project::parse_list(original).unwrap(), Some(file.clone()));
        let result = store.update("chunk", |project| {
            project.name = "Should not be published".into();
            // Replacing the destination with a directory makes the atomic replacement fail.
            std::fs::remove_file(&file).unwrap();
            std::fs::create_dir(&file).unwrap();
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(store.list().ok().unwrap()[0].name, "chunk");
        let read_only = Store::new(Project::parse_list(original).unwrap(), None);
        assert!(read_only.update("chunk", |_| Ok(())).is_err());
    }
}
