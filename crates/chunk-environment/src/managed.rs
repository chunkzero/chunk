//! Serving the deployments the management service asks for, through `EnvironmentService.Attach` and `ReportStatus`.

mod release;

use crate::{Core, Gateway, GatewayConfig};
use chunk_management::{Client, Code, v1};
use std::{io, path::PathBuf, sync::OnceLock, time::Duration};

const REATTACH: Duration = Duration::from_secs(5);
/// Why a deployment failed, as reported, is cut to this many bytes.
const MAX_MESSAGE_BYTES: usize = 1024;

pub struct ManagementConfig {
    /// The management service's base URL.
    pub url: String,
    /// The environment's bearer token.
    pub token: String,
}

/// Core's attachment to the management service: it follows the desired state and reports deployment progress.
pub(crate) struct Managed<'a> {
    client: Client,
    /// Unique to this run of the process.
    instance_id: String,
    environment: String,
    releases: PathBuf,
    core: &'a Core,
    gateway: &'a OnceLock<Gateway>,
    /// The gateway to start once a deployment is active.
    gateway_config: Option<GatewayConfig>,
    /// The deployment players are routed to.
    serving: Option<String>,
    /// The latest deployment this core rejected, and why.
    rejected: Option<(String, String)>,
    /// Deployments whose JVMs or backend version are not yet stopped.
    retiring: Vec<String>,
    lease: u64,
    sequence: u64,
}

/// Why following the desired state stopped.
enum Interrupted {
    /// Attaching again may succeed.
    Retry(chunk_management::Error),
    /// This core must stop serving.
    Fatal(io::Error),
}

impl From<chunk_management::Error> for Interrupted {
    fn from(error: chunk_management::Error) -> Self {
        if error.code() == Code::FailedPrecondition {
            Self::Fatal(io::Error::other(format!("management fenced this core: {error}")))
        } else {
            Self::Retry(error)
        }
    }
}

impl<'a> Managed<'a> {
    pub(crate) fn new(
        config: ManagementConfig,
        environment: String,
        releases: PathBuf,
        core: &'a Core,
        gateway: &'a OnceLock<Gateway>,
        gateway_config: Option<GatewayConfig>,
    ) -> Self {
        Self {
            client: Client::new(config.url).with_token(config.token),
            instance_id: uuid::Uuid::new_v4().to_string(),
            environment,
            releases,
            core,
            gateway,
            gateway_config,
            serving: None,
            rejected: None,
            retiring: Vec::new(),
            lease: 0,
            sequence: 0,
        }
    }

    /// Attaches until management fences this core or it cannot serve, attaching again after other interruptions.
    pub(crate) async fn run(mut self) -> io::Error {
        loop {
            match self.attach().await {
                Ok(()) => tracing::warn!("management ended the attach"),
                Err(Interrupted::Retry(error)) => tracing::warn!(%error, "management attach interrupted"),
                Err(Interrupted::Fatal(error)) => return error,
            }
            tokio::time::sleep(REATTACH).await;
        }
    }

    async fn attach(&mut self) -> Result<(), Interrupted> {
        let request = v1::AttachRequest {
            instance_id: self.instance_id.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            core: true,
            epoch: self.core.epoch().map_err(Interrupted::Fatal)?,
        };
        let mut stream = self.client.attach(&request).await?;
        let mut applied = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            let desired = tokio::select! {
                desired = stream.message() => desired?,
                _ = tick.tick() => {
                    self.retire().await;
                    continue;
                }
            };
            let Some(desired) = desired else { return Ok(()) };
            if desired.lease != self.lease {
                (self.lease, self.sequence) = (desired.lease, 0);
            }
            // A resent revision is a keepalive.
            if applied != Some(desired.revision) {
                self.apply(&desired).await?;
                applied = Some(desired.revision);
            }
        }
    }

    /// Serves `desired`'s deployment unless it already serves or rejected it, then reports its progress.
    async fn apply(&mut self, desired: &v1::AttachResponse) -> Result<(), Interrupted> {
        if desired.environment_id != self.environment {
            return Err(Interrupted::Fatal(io::Error::other(format!(
                "management attached this core to environment {:?}, not {:?}",
                desired.environment_id, self.environment
            ))));
        }
        let deployment = desired.deployment_id.as_str();
        if deployment.is_empty() {
            return self.report(desired, None).await;
        }
        let (state, message) = if self.serving.as_deref() == Some(deployment) {
            (v1::DeploymentState::Active, String::new())
        } else if let Some((_, message)) = self.rejected.as_ref().filter(|(rejected, _)| rejected == deployment) {
            (v1::DeploymentState::Failed, message.clone())
        } else {
            self.report(desired, Some(progress(deployment, v1::DeploymentState::InProgress, String::new()))).await?;
            match self.deploy(desired).await {
                Ok(()) => {
                    self.route(deployment).await.map_err(Interrupted::Fatal)?;
                    (v1::DeploymentState::Active, String::new())
                }
                Err(error) => {
                    tracing::warn!(%error, deployment, "deployment rejected; the previous one keeps serving");
                    let message = bounded(error.to_string());
                    self.rejected = Some((deployment.into(), message.clone()));
                    (v1::DeploymentState::Failed, message)
                }
            }
        };
        self.report(desired, Some(progress(deployment, state, message))).await
    }

    /// Loads the deployment's release, makes it resident in the backend, and makes it control's current release.
    async fn deploy(&mut self, desired: &v1::AttachResponse) -> io::Result<()> {
        let artifact = desired.release.as_ref().ok_or_else(|| io::Error::other("the deployment names no release"))?;
        let loaded = release::load(&self.client, &self.releases, artifact).await?;
        let deployment = &desired.deployment_id;
        self.core.deploy(loaded.bundle(deployment)).await?;
        let activated =
            self.core.activate(deployment, loaded.distribution(), loaded.control(&self.environment, deployment));
        if activated.is_err() {
            self.retiring.push(deployment.clone());
        }
        activated
    }

    /// Sends later player connections to `deployment`, starting the gateway for the first one, and retires the
    /// deployment served before.
    async fn route(&mut self, deployment: &str) -> io::Result<()> {
        let mut target = self.core.target()?;
        target.backend.deployment = deployment.into();
        if let Some(gateway) = self.gateway.get() {
            gateway.retarget(target)?;
        } else if let Some(config) = self.gateway_config.take() {
            _ = self.gateway.set(Gateway::start(config, target).await?);
        }
        self.retiring.extend(self.serving.replace(deployment.into()));
        Ok(())
    }

    /// Stops retired deployments' JVMs, then releases their backend versions once every JVM has exited.
    async fn retire(&mut self) {
        if self.retiring.is_empty() {
            return;
        }
        let (Ok(control), Some(backend)) = (self.core.control(), self.core.backend()) else { return };
        let mut retiring = Vec::new();
        for deployment in std::mem::take(&mut self.retiring) {
            let stopped = control.retire_release(&deployment).unwrap_or_else(|error| {
                tracing::warn!(%error, deployment, "release not yet retired");
                false
            });
            if !stopped || !released(&backend, &deployment).await {
                retiring.push(deployment);
            }
        }
        self.retiring = retiring;
    }

    async fn report(
        &mut self,
        desired: &v1::AttachResponse,
        deployment: Option<v1::DeploymentProgress>,
    ) -> Result<(), Interrupted> {
        self.sequence += 1;
        let request = v1::ReportStatusRequest {
            observe_time: Some(std::time::SystemTime::now().into()),
            deployment,
            lease: self.lease,
            sequence: self.sequence,
            desired_revision: desired.revision,
            ..Default::default()
        };
        self.client.report_status(&request).await?;
        Ok(())
    }
}

fn progress(deployment: &str, state: v1::DeploymentState, message: String) -> v1::DeploymentProgress {
    v1::DeploymentProgress { deployment_id: deployment.into(), state: state.into(), message }
}

/// Whether the backend no longer holds `deployment`, retrying later only while it is busy.
async fn released(backend: &chunk_backend::Backend, deployment: &str) -> bool {
    let Ok(id) = chunk_backend::DeploymentId::new(deployment) else { return true };
    match backend.release(id).await {
        Err(chunk_backend::Error::Busy) => false,
        Err(error) => {
            tracing::warn!(%error, deployment, "backend version not released");
            true
        }
        Ok(_) => true,
    }
}

fn bounded(mut message: String) -> String {
    message.truncate(message.floor_char_boundary(MAX_MESSAGE_BYTES));
    message
}

#[cfg(test)]
mod tests;
