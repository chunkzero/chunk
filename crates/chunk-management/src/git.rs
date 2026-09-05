//! Live branch listing from a Git remote, so the dashboard can offer real branch names.

use super::projects::{ApiError, ErrorBody, validate_repository};
use axum::{Json, extract::Query, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::{process::Stdio, time::Duration};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct RepositoryQuery {
    repository: String,
}

#[derive(Serialize, utoipa::ToSchema)]
pub(super) struct Branches {
    /// The branch the remote's HEAD points at, when the remote advertises one.
    default_branch: Option<String>,
    branches: Vec<String>,
}

#[utoipa::path(get, path = "/git/branches", params(RepositoryQuery),
    responses((status = 200, body = Branches), (status = 400, body = ErrorBody), (status = 401), (status = 502, body = ErrorBody), (status = 503, body = ErrorBody), (status = 504, body = ErrorBody)))]
pub(super) async fn list_branches(Query(query): Query<RepositoryQuery>) -> Result<Json<Branches>, ApiError> {
    let repository = query.repository.trim();
    validate_repository(repository)?;
    let command = tokio::process::Command::new("git")
        .args(["ls-remote", "--symref", "--", repository, "HEAD", "refs/heads/*"])
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(20), command)
        .await
        .map_err(|_| ApiError::new(StatusCode::GATEWAY_TIMEOUT, "The repository did not respond in time."))?
        .map_err(|error| {
            tracing::warn!(%error, "could not run git");
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "Git is not available on this server.")
        })?;
    if !output.status.success() {
        tracing::info!(
            stderr = %String::from_utf8_lossy(&output.stderr).trim(),
            "git ls-remote failed"
        );
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "The repository could not be reached. Check the URL and access.",
        ));
    }
    Ok(Json(parse(&String::from_utf8_lossy(&output.stdout))))
}

fn parse(output: &str) -> Branches {
    let mut default_branch = None;
    let mut branches = Vec::new();
    for line in output.lines() {
        let Some((left, right)) = line.split_once('\t') else {
            continue;
        };
        if right == "HEAD" {
            if let Some(target) = left.strip_prefix("ref: refs/heads/") {
                default_branch = Some(target.to_owned());
            }
        } else if let Some(name) = right.strip_prefix("refs/heads/") {
            branches.push(name.to_owned());
        }
    }
    branches.sort_unstable();
    Branches {
        default_branch,
        branches,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_symref_and_heads() {
        let output = "ref: refs/heads/main\tHEAD\n\
            0123abcd\tHEAD\n\
            0123abcd\trefs/heads/main\n\
            4567efab\trefs/heads/feat/dashboard\n\
            89ab0123\trefs/tags/v1\n";
        let branches = parse(output);
        assert_eq!(branches.default_branch.as_deref(), Some("main"));
        assert_eq!(branches.branches, ["feat/dashboard", "main"]);
        assert!(parse("").default_branch.is_none());
    }
}
