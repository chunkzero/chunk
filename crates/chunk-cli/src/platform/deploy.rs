//! `chunk deploy`, `chunk promote` and `chunk rollback`: make a release current in an environment and follow the
//! deployment. Deploy first builds the release and uploads it unless the project holds it.

use std::{
    collections::BTreeSet,
    fmt::Write as _,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use chunk_management::{
    Client, Code,
    v1::{
        CompleteReleaseUploadRequest, DeployRequest, Deployment, DeploymentState, Environment, GetDeploymentRequest,
        PromoteRequest, RollbackRequest, UploadReleaseRequest,
    },
};
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use super::{EnvironmentArgs, Session, api_error, resources::request_id, secrets};
use crate::building::{self, BuildMode, progress::Progress};

#[derive(clap::Args)]
pub(crate) struct Options {
    /// The project directory to build.
    #[arg(default_value = ".")]
    path: PathBuf,
    #[command(flatten)]
    environment: EnvironmentArgs,
    /// Stop the deployments this one replaces at once, disconnecting their players, instead of draining them.
    #[arg(long)]
    stop_previous: bool,
}

#[derive(clap::Args)]
pub(crate) struct Promote {
    #[command(flatten)]
    environment: EnvironmentArgs,
    /// The environment, of the same project, whose active release to deploy to `--env`.
    #[arg(long)]
    from: String,
}

#[derive(clap::Args)]
pub(crate) struct Rollback {
    #[command(flatten)]
    environment: EnvironmentArgs,
    /// The earlier deployment, by ID, whose release to restore; defaults to the one active before the current one.
    #[arg(long)]
    to: Option<String>,
}

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const DEPLOY_ATTEMPTS: u64 = 4;

pub(super) async fn run(options: Options) -> io::Result<()> {
    let session = Session::open()?;
    let (project, environment) = session.environment(&options.environment).await?;
    chunk_service::run(|stop| async move {
        let local = building::prepare(&building::Options { project: options.path, output: None, frozen: false })?;
        cliclack::log::info("Building application release…")?;
        let built = building::execute(&local, BuildMode::Release, stop.clone(), Progress::default()).await?;
        let client = &session.client;
        warn_missing_secrets(client, &environment, &built.release.directory).await?;
        let release = built.release.id;
        let archive = built.release.archive.expect("release builds write an archive");
        let deployment = tokio::select! {
            deployment = async {
                upload(client, &project.id, &release, archive).await?;
                deploy(client, &environment, &release, options.stop_previous).await
            } => deployment?,
            () = stop.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "deploy stopped")),
        };
        follow_until_stopped(client, &environment, deployment, stop).await
    })
    .await
}

/// Promotes the release active in `--from` to `--env`, and follows the deployment.
pub(super) async fn promote(options: Promote) -> io::Result<()> {
    let session = Session::open()?;
    let (project, target) = session.environment(&options.environment).await?;
    let source = session.environment_in(&project, &options.from).await?;
    let request = PromoteRequest {
        request_id: request_id(),
        source_environment_id: source.id,
        target_environment_id: target.id.clone(),
    };
    let client = &session.client;
    let deployment = retry(|| client.promote(&request)).await?.deployment;
    let deployment = deployment.ok_or_else(|| io::Error::other("Promote returned no deployment"))?;
    chunk_service::run(|stop| follow_until_stopped(client, &target, deployment, stop)).await
}

/// Deploys an earlier deployment's release again, and follows the deployment.
pub(super) async fn rollback(options: Rollback) -> io::Result<()> {
    let session = Session::open()?;
    let (_, environment) = session.environment(&options.environment).await?;
    let request = RollbackRequest {
        request_id: request_id(),
        environment_id: environment.id.clone(),
        deployment_id: options.to.unwrap_or_default(),
    };
    let client = &session.client;
    let deployment = retry(|| client.rollback(&request)).await?.deployment;
    let deployment = deployment.ok_or_else(|| io::Error::other("Rollback returned no deployment"))?;
    chunk_service::run(|stop| follow_until_stopped(client, &environment, deployment, stop)).await
}

/// Follows the deployment until `stop`, which leaves it running.
async fn follow_until_stopped(
    client: &Client,
    environment: &Environment,
    deployment: Deployment,
    stop: CancellationToken,
) -> io::Result<()> {
    let id = deployment.id.clone();
    tokio::select! {
        followed = follow(client, environment, deployment) => followed,
        () = stop.cancelled() => {
            cliclack::log::info(format!(
                "Stopped waiting; deployment {id} continues. `chunk deployments --env {}` shows it.",
                environment.name
            ))?;
            Err(io::Error::new(io::ErrorKind::Interrupted, "stopped waiting"))
        }
    }
}

async fn upload(client: &Client, project_id: &str, release_id: &str, archive: PathBuf) -> io::Result<()> {
    let (bytes, sha256) = tokio::task::spawn_blocking(move || {
        let bytes = std::fs::read(&archive)?;
        let mut sha256 = String::with_capacity(64);
        for byte in Sha256::digest(&bytes) {
            write!(sha256, "{byte:02x}").expect("writing to a string");
        }
        Ok::<_, io::Error>((bytes, sha256))
    })
    .await
    .map_err(io::Error::other)??;
    let size = u64::try_from(bytes.len()).map_err(io::Error::other)?;
    let declared = UploadReleaseRequest {
        project_id: project_id.into(),
        release_id: release_id.into(),
        archive_sha256: sha256,
        archive_size_bytes: size,
    };
    let response = client.upload_release(&declared).await.map_err(api_error)?;
    if let Some(target) = response.upload {
        cliclack::log::info(format!("Uploading release {} ({})…", short(release_id), mebibytes(size)))?;
        client.upload_archive(&target, bytes).await.map_err(api_error)?;
    } else {
        cliclack::log::info(format!("The platform already holds release {}", short(release_id)))?;
    }
    let complete = CompleteReleaseUploadRequest { project_id: project_id.into(), release_id: release_id.into() };
    client.complete_release_upload(&complete).await.map_err(api_error)?;
    Ok(())
}

/// Warns about the secrets the release requires that the environment has no value for; actions that read them fail.
async fn warn_missing_secrets(client: &Client, environment: &Environment, release: &Path) -> io::Result<()> {
    let backend: chunk_contract::Deployment = chunk_service::read(&release.join("backend.json"))?;
    let required = backend.contracts.env.secrets;
    if required.is_empty() {
        return Ok(());
    }
    let set: BTreeSet<String> =
        secrets::list(client, &environment.id).await?.into_iter().map(|secret| secret.name).collect();
    let missing: Vec<_> = required.difference(&set).map(String::as_str).collect();
    if missing.is_empty() {
        return Ok(());
    }
    cliclack::log::warning(format!(
        "{} has no value for required secrets {}; set them with `chunk secrets put NAME --env {}`",
        environment.name,
        missing.join(", "),
        environment.name
    ))
}

/// Deploys once however often an unreachable platform makes it retry, by reusing one request ID.
async fn deploy(
    client: &Client,
    environment: &Environment,
    release_id: &str,
    stop_previous: bool,
) -> io::Result<Deployment> {
    let request = DeployRequest {
        request_id: request_id(),
        environment_id: environment.id.clone(),
        release_id: release_id.into(),
        stop_previous,
        asset_revision_id: String::new(),
    };
    let response = retry(|| client.deploy(&request)).await?;
    response.deployment.ok_or_else(|| io::Error::other("Deploy returned no deployment"))
}

/// Makes a call that takes a request ID, repeating it while an unreachable platform makes it fail as unavailable.
async fn retry<T, Call>(call: impl Fn() -> Call) -> io::Result<T>
where
    Call: Future<Output = Result<T, chunk_management::Error>>,
{
    let mut attempt = 1;
    loop {
        match call().await {
            Ok(response) => return Ok(response),
            Err(error) if error.code() == Code::Unavailable && attempt < DEPLOY_ATTEMPTS => {
                tokio::time::sleep(Duration::from_secs(attempt)).await;
                attempt += 1;
            }
            Err(error) => return Err(api_error(error)),
        }
    }
}

/// Prints the deployment's states until it is active, failed or superseded.
async fn follow(client: &Client, environment: &Environment, mut deployment: Deployment) -> io::Result<()> {
    cliclack::log::info(format!(
        "Deploying release {} to {} as {}",
        short(&deployment.release_id),
        environment.name,
        deployment.id
    ))?;
    let mut shown = DeploymentState::Unspecified;
    loop {
        let state = deployment.state();
        if state != shown {
            cliclack::log::step(super::resources::label(state.as_str_name(), "DEPLOYMENT_STATE_"))?;
            shown = state;
        }
        match state {
            DeploymentState::Active => {
                let place = if environment.join_address.is_empty() {
                    String::new()
                } else {
                    format!("; players join at {}", environment.join_address)
                };
                return cliclack::log::success(format!("Deployed to {}{place}", environment.name));
            }
            DeploymentState::Failed => {
                return Err(io::Error::other(format!("The deployment failed: {}", deployment.message)));
            }
            DeploymentState::Superseded => {
                return Err(io::Error::other("A later deployment superseded this one before it became active."));
            }
            _ => {}
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        let request = GetDeploymentRequest { deployment_id: deployment.id.clone() };
        match client.get_deployment(&request).await {
            Ok(response) => {
                deployment = response.deployment.ok_or_else(|| io::Error::other("GetDeployment returned none"))?;
            }
            // A restarting or slow platform answers again shortly; the deployment goes on without it.
            Err(error) if error.code() == Code::Unavailable => {}
            Err(error) => return Err(api_error(error)),
        }
    }
}

fn short(release_id: &str) -> &str {
    release_id.get(..12).unwrap_or(release_id)
}

fn mebibytes(bytes: u64) -> String {
    const MIB: u64 = 1024 * 1024;
    format!("{}.{} MiB", bytes / MIB, bytes % MIB * 10 / MIB)
}
