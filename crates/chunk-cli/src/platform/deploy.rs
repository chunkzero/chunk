//! `chunk deploy`: build the release, upload it unless the project holds it, deploy it and follow the deployment.

use std::{fmt::Write as _, io, path::PathBuf, time::Duration};

use chunk_management::{
    Client, Code,
    v1::{
        CompleteReleaseUploadRequest, DeployRequest, Deployment, DeploymentState, Environment, GetDeploymentRequest,
        UploadReleaseRequest,
    },
};
use sha2::{Digest, Sha256};

use super::{EnvironmentArgs, Session, api_error};
use crate::building::{self, BuildMode, progress::Progress};

#[derive(clap::Args)]
pub(crate) struct Options {
    /// The project directory to build.
    #[arg(default_value = ".")]
    path: PathBuf,
    #[command(flatten)]
    environment: EnvironmentArgs,
}

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const DEPLOY_ATTEMPTS: u64 = 4;

pub(super) async fn run(options: Options) -> io::Result<()> {
    let session = Session::open()?;
    let (project, environment) = session.environment(&options.environment).await?;
    chunk_service::run(|stop| async move {
        let local = building::prepare(&building::Options { project: options.path, output: None })?;
        cliclack::log::info("Building application release…")?;
        let built = building::execute(&local, BuildMode::Release, stop.clone(), Progress::default()).await?;
        let release = built.release.id;
        let archive = built.release.archive.expect("release builds write an archive");
        let client = &session.client;
        let deployment = tokio::select! {
            deployment = async {
                upload(client, &project.id, &release, archive).await?;
                deploy(client, &environment, &release).await
            } => deployment?,
            () = stop.cancelled() => return Err(io::Error::new(io::ErrorKind::Interrupted, "deploy stopped")),
        };
        let id = deployment.id.clone();
        tokio::select! {
            followed = follow(client, &environment, deployment) => followed,
            () = stop.cancelled() => {
                cliclack::log::info(format!(
                    "Stopped waiting; deployment {id} continues. `chunk deployments --env {}` shows it.",
                    environment.name
                ))?;
                Err(io::Error::new(io::ErrorKind::Interrupted, "stopped waiting"))
            }
        }
    })
    .await
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

/// Deploys once however often an unreachable platform makes it retry, by reusing one request ID.
async fn deploy(client: &Client, environment: &Environment, release_id: &str) -> io::Result<Deployment> {
    let request = DeployRequest {
        request_id: uuid::Uuid::new_v4().to_string(),
        environment_id: environment.id.clone(),
        release_id: release_id.into(),
    };
    let mut attempt = 1;
    loop {
        match client.deploy(&request).await {
            Ok(response) => {
                return response.deployment.ok_or_else(|| io::Error::other("Deploy returned no deployment"));
            }
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
                let place = if environment.hostname.is_empty() {
                    String::new()
                } else {
                    format!("; players join at {}", environment.hostname)
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
