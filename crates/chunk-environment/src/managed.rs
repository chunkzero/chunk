//! Serving the deployments the management service asks for, through `EnvironmentService.Attach` and `ReportStatus`.

mod activation;
mod launcher;
mod release;
mod retire;
mod status;

use crate::{Core, Gateway, GatewayConfig};
use activation::Activation;
use chunk_management::{Client, Code, v1};
pub(crate) use launcher::{Lease, ManagementLauncher};
use std::{
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Mutex, MutexGuard, OnceLock, PoisonError},
    time::Duration,
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const REATTACH: Duration = Duration::from_secs(5);
/// Management resends the desired state at least every 30 seconds, so a quieter stream is stalled.
const ATTACH_IDLE: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Why a deployment failed, as reported, is cut to this many bytes.
const MAX_MESSAGE_BYTES: usize = 1024;

pub struct ManagementConfig {
    /// The management service's base URL.
    pub url: String,
    /// The environment's bearer token.
    pub token: String,
}

impl ManagementConfig {
    pub(crate) fn client(&self) -> Client {
        Client::new(&self.url).with_token(&self.token)
    }
}

/// Core's attachment to the management service: it follows the desired state and reports its status, including
/// deployment progress.
pub(crate) struct Managed<'a> {
    client: Client,
    /// Where core's hold on the environment is published, for the launcher's calls.
    lease: watch::Sender<Lease>,
    /// Unique to this run of the process.
    instance_id: String,
    environment: String,
    releases: release::Store,
    /// Where the activation management has not yet accepted is recorded.
    activation: PathBuf,
    core: &'a Core,
    gateway: &'a OnceLock<Gateway>,
    /// The gateway to start once a deployment is active.
    gateway_config: Option<GatewayConfig>,
    deployments: Mutex<Deployments>,
    reporter: status::Reporter,
    /// Where edges reach the gateway.
    private_address: status::PrivateAddress,
    /// Cancelled once core shuts down. From then on no deployment activates, no gateway starts and no status is
    /// reported periodically, while attaches still publish their leases.
    stopping: CancellationToken,
}

#[derive(Default)]
struct Deployments {
    /// The revision of the latest desired state this process received.
    revision: u64,
    /// The deployment of the latest desired state this process received; unset until the first one arrives.
    desired: Option<String>,
    /// The deployment players are routed to.
    serving: Option<String>,
    /// The activation management has not yet accepted, as recorded at `Managed::activation`.
    unacknowledged: Option<Activation>,
    /// The deployment being loaded.
    loading: Option<String>,
    /// The latest deployment this core rejected, and why.
    rejected: Option<(String, String)>,
}

impl Deployments {
    /// Whether management may still ask for `deployment` or players may still be routed to it, so it must not retire.
    /// Before the first desired state arrives, every deployment is kept.
    fn kept(&self, deployment: &str) -> bool {
        let Some(desired) = &self.desired else { return true };
        let previous = self.unacknowledged.as_ref().and_then(|pending| pending.predecessor.as_deref());
        [Some(desired.as_str()), self.serving.as_deref(), previous, self.loading.as_deref()].contains(&Some(deployment))
    }
}

/// Why following the desired state stopped.
enum Interrupted {
    /// Attaching again may succeed.
    Retry(io::Error),
    /// This core must stop serving.
    Fatal(io::Error),
    /// Another core superseded this one, which must stop serving.
    Fenced(io::Error),
}

impl From<chunk_management::Error> for Interrupted {
    fn from(error: chunk_management::Error) -> Self {
        if error.code() == Code::FailedPrecondition {
            Self::Fenced(io::Error::other(format!("management fenced this core: {error}")))
        } else {
            Self::Retry(io::Error::other(error))
        }
    }
}

type Future<'a> = Pin<Box<dyn std::future::Future<Output = Result<(), Interrupted>> + Send + 'a>>;

/// The work of applying one desired revision. Dropping it stops the work.
struct Work<'a> {
    revision: u64,
    deployment: String,
    cancel: CancellationToken,
    future: Future<'a>,
    deployments: &'a Mutex<Deployments>,
}

impl Drop for Work<'_> {
    fn drop(&mut self) {
        lock(self.deployments).loading = None;
    }
}

impl<'a> Managed<'a> {
    pub(crate) fn new(
        management: &ManagementConfig,
        lease: watch::Sender<Lease>,
        environment: String,
        state: &Path,
        core: &'a Core,
        gateway: &'a OnceLock<Gateway>,
        gateway_config: Option<GatewayConfig>,
    ) -> Self {
        let client = management.client();
        Self {
            reporter: status::Reporter::new(client.clone()),
            private_address: status::PrivateAddress::new(core.private_address(), &management.url),
            client,
            lease,
            instance_id: uuid::Uuid::new_v4().to_string(),
            environment,
            releases: release::Store::new(state, core.archives().clone()),
            activation: state.join("managed.json"),
            core,
            gateway,
            gateway_config,
            deployments: Mutex::default(),
            stopping: CancellationToken::new(),
        }
    }

    /// The token that tells this attachment core is shutting down.
    pub(crate) fn stopping(&self) -> CancellationToken {
        self.stopping.clone()
    }

    /// Attaches until management fences this core or it cannot serve, attaching again after other interruptions.
    /// Meanwhile, it retires the deployments it no longer serves and reports its status.
    pub(crate) async fn run(self) -> io::Error {
        if let Err(error) = self.recover() {
            return error;
        }
        if let Err(error) = release::sweep(&self.releases).await {
            tracing::warn!(%error, "abandoned release downloads not removed");
        }
        let restored = match self.core.control().and_then(|control| control.release_ids().map_err(io::Error::other)) {
            Ok(retained) => release::restore(&self.releases, retained).await,
            Err(error) => Err(error),
        };
        if let Err(error) = restored {
            tracing::warn!(%error, "kept release archives not restored");
        }
        tokio::select! {
            error = self.follow() => error,
            never = self.reclaim() => match never {},
            never = self.reporter.keep_reporting(|| self.current()) => match never {},
        }
    }

    /// Protects an activation an earlier run recorded until management accepts it, if control made it current.
    fn recover(&self) -> io::Result<()> {
        let Some(recorded) = activation::read(&self.activation)? else { return Ok(()) };
        let current = self.core.control()?.current_release().map_err(io::Error::other)?;
        if current.as_ref() == Some(&recorded.activated) {
            lock(&self.deployments).unacknowledged = Some(recorded);
            Ok(())
        } else {
            activation::clear(&self.activation)
        }
    }

    async fn follow(&self) -> io::Error {
        loop {
            match self.attach().await {
                Ok(()) => tracing::warn!("management ended the attach"),
                Err(Interrupted::Retry(error)) => tracing::warn!(%error, "management attach interrupted"),
                Err(Interrupted::Fatal(error)) => return error,
                Err(Interrupted::Fenced(error)) => {
                    self.lease.send_replace(Lease::Superseded);
                    return error;
                }
            }
            tokio::time::sleep(REATTACH).await;
        }
    }

    /// Applies each new desired revision beside the stream, so a newer one or a fence is seen at once. A newer
    /// revision naming another deployment cancels the work on the one before.
    async fn attach(&self) -> Result<(), Interrupted> {
        let request = v1::AttachRequest {
            instance_id: self.instance_id.clone(),
            version: env!("CARGO_PKG_VERSION").into(),
            core: true,
            epoch: self.core.epoch().map_err(Interrupted::Fatal)?,
        };
        let mut stream = deadline(REQUEST_TIMEOUT, self.client.attach(&request)).await?;
        let (mut latest, mut applied, mut work) = (None::<v1::AttachResponse>, None, None::<Work>);
        loop {
            if work.is_none()
                && !self.stopping.is_cancelled()
                && let Some(desired) = latest.as_ref().filter(|desired| applied != Some(desired.revision))
            {
                work = Some(self.start(desired.clone()));
            }
            let running = async {
                match &mut work {
                    Some(work) => work.future.as_mut().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                message = deadline(ATTACH_IDLE, stream.message()) => {
                    let Some(desired) = message? else { return Ok(()) };
                    self.check(&desired)?;
                    self.lease.send_replace(Lease::Held(desired.lease));
                    {
                        let mut deployments = lock(&self.deployments);
                        deployments.revision = desired.revision;
                        deployments.desired = Some(desired.deployment_id.clone());
                    }
                    if let Some(work) = work.as_ref().filter(|work| work.deployment != desired.deployment_id) {
                        work.cancel.cancel();
                    }
                    latest = Some(desired);
                }
                finished = running => {
                    finished?;
                    applied = work.take().map(|work| work.revision);
                }
            }
        }
    }

    fn check(&self, desired: &v1::AttachResponse) -> Result<(), Interrupted> {
        if desired.environment_id == self.environment {
            return Ok(());
        }
        Err(Interrupted::Fatal(io::Error::other(format!(
            "management attached this core to environment {:?}, not {:?}",
            desired.environment_id, self.environment
        ))))
    }

    fn start(&self, desired: v1::AttachResponse) -> Work<'_> {
        lock(&self.deployments).loading = Some(desired.deployment_id.clone());
        let cancel = self.stopping.child_token();
        Work {
            revision: desired.revision,
            deployment: desired.deployment_id.clone(),
            cancel: cancel.clone(),
            future: Box::pin(self.apply(desired, cancel)),
            deployments: &self.deployments,
        }
    }

    /// Serves `desired`'s deployment unless it already serves or rejected it, then reports its progress. Work that
    /// `cancel` superseded ends without switching traffic or reporting. First, it reports an earlier activation
    /// management has not accepted yet, since the deployment served before it is kept until then.
    async fn apply(&self, desired: v1::AttachResponse, cancel: CancellationToken) -> Result<(), Interrupted> {
        let deployment = desired.deployment_id.as_str();
        let unacknowledged = lock(&self.deployments).unacknowledged.as_ref().map(|pending| pending.activated.clone());
        if let Some(active) = unacknowledged.filter(|active| active != deployment) {
            self.report(&desired, Some(progress(&active, v1::DeploymentState::Active, String::new()))).await?;
        }
        if deployment.is_empty() {
            return self.report(&desired, None).await;
        }
        let known = {
            let deployments = lock(&self.deployments);
            if deployments.serving.as_deref() == Some(deployment) {
                Some((v1::DeploymentState::Active, String::new()))
            } else {
                let rejected = deployments.rejected.as_ref().filter(|(rejected, _)| rejected == deployment);
                rejected.map(|(_, message)| (v1::DeploymentState::Failed, message.clone()))
            }
        };
        let (state, message) = if let Some(known) = known {
            known
        } else {
            self.report(&desired, Some(progress(deployment, v1::DeploymentState::InProgress, String::new()))).await?;
            match self.deploy(&desired, &cancel).await {
                Ok(false) => return Ok(()),
                Ok(true) => {
                    self.route(deployment).await.map_err(Interrupted::Fatal)?;
                    (v1::DeploymentState::Active, String::new())
                }
                Err(error) => {
                    tracing::warn!(%error, deployment, "deployment rejected; the previous one keeps serving");
                    let message = bounded(error.to_string());
                    lock(&self.deployments).rejected = Some((deployment.into(), message.clone()));
                    (v1::DeploymentState::Failed, message)
                }
            }
        };
        self.report(&desired, Some(progress(deployment, state, message))).await
    }

    /// Loads the deployment's release, makes it resident in the backend, and, unless `cancel` superseded it by then,
    /// makes it control's current release. Returns whether it did.
    async fn deploy(&self, desired: &v1::AttachResponse, cancel: &CancellationToken) -> io::Result<bool> {
        let artifact = desired.release.as_ref().ok_or_else(|| io::Error::other("the deployment names no release"))?;
        let Some(loaded) = release::load(&self.client, &self.releases, artifact, cancel).await? else {
            return Ok(false);
        };
        let deployment = &desired.deployment_id;
        if cancel.is_cancelled() {
            return Ok(false);
        }
        self.core.deploy(loaded.bundle(deployment)).await?;
        // Nothing awaits between this check and switching traffic, so the attach loop cannot supersede it meanwhile.
        if cancel.is_cancelled() {
            return Ok(false);
        }
        // Recorded first, so a crash once control has activated it still protects the predecessor. Activating the
        // unaccepted deployment again keeps its predecessor.
        let current = self.core.control()?.current_release().map_err(io::Error::other)?;
        let predecessor = match &lock(&self.deployments).unacknowledged {
            Some(pending) if pending.activated == *deployment => pending.predecessor.clone(),
            _ => current.filter(|current| current != deployment),
        };
        let pending = Activation { predecessor, activated: deployment.clone() };
        activation::write(&self.activation, &pending)?;
        let release = loaded.control(&self.environment, deployment);
        if let Err(error) = self.core.activate(release) {
            _ = activation::clear(&self.activation);
            return Err(error);
        }
        lock(&self.deployments).unacknowledged = Some(pending);
        Ok(true)
    }

    /// Sends later player connections to `deployment`, starting the gateway for the first one unless core is stopping.
    async fn route(&self, deployment: &str) -> io::Result<()> {
        let mut target = self.core.target()?;
        target.deployment = deployment.into();
        if let Some(gateway) = self.gateway.get() {
            gateway.retarget(target)?;
        } else if let Some(config) = &self.gateway_config {
            tokio::select! {
                biased;
                () = self.stopping.cancelled() => {}
                started = Gateway::start(config.clone(), target) => {
                    _ = self.gateway.set(started?);
                }
            }
        }
        lock(&self.deployments).serving = Some(deployment.into());
        Ok(())
    }

    /// Reports status under `desired`'s lease. Once management accepts an activation, the deployment served before it
    /// may retire.
    async fn report(
        &self,
        desired: &v1::AttachResponse,
        deployment: Option<v1::DeploymentProgress>,
    ) -> Result<(), Interrupted> {
        let active = deployment
            .as_ref()
            .filter(|progress| progress.state() == v1::DeploymentState::Active)
            .map(|progress| progress.deployment_id.clone());
        let observed = self.observe(desired.lease, desired.revision).await;
        self.reporter.send(observed, deployment).await?;
        let mut deployments = lock(&self.deployments);
        if let Some(active) = active
            && deployments.unacknowledged.as_ref().is_some_and(|pending| pending.activated == active)
        {
            if let Err(error) = activation::clear(&self.activation) {
                tracing::warn!(%error, "accepted activation still recorded");
            }
            deployments.unacknowledged = None;
        }
        Ok(())
    }

    /// What to report periodically under the latest attach's lease, unless core is stopping or was superseded.
    async fn current(&self) -> Option<status::Observed> {
        if self.stopping.is_cancelled() {
            return None;
        }
        let Lease::Held(lease) = *self.lease.borrow() else { return None };
        let revision = lock(&self.deployments).revision;
        Some(self.observe(lease, revision).await)
    }

    /// Core's status under `lease` and `revision`. A gateway bound to every interface is reported at the machine's
    /// private address.
    async fn observe(&self, lease: u64, revision: u64) -> status::Observed {
        let mut gateway_addresses = Vec::new();
        if let Some(gateway) = self.gateway.get() {
            let bound = gateway.address();
            let address =
                if bound.ip().is_unspecified() { self.private_address.find().await } else { Some(bound.ip()) };
            gateway_addresses.extend(address.map(|address| SocketAddr::new(address, bound.port()).to_string()));
        }
        let online_players =
            match self.core.control().and_then(|control| control.online_players().map_err(io::Error::other)) {
                Ok(online) => u32::try_from(online).unwrap_or(u32::MAX),
                Err(error) => {
                    tracing::warn!(%error, "online players unknown");
                    0
                }
            };
        status::Observed { lease, revision, gateway_addresses, online_players }
    }
}

/// `call`'s result, or a retryable interruption once `limit` passes.
async fn deadline<T>(
    limit: Duration,
    call: impl std::future::Future<Output = Result<T, chunk_management::Error>>,
) -> Result<T, Interrupted> {
    match tokio::time::timeout(limit, call).await {
        Ok(result) => Ok(result?),
        Err(_) => Err(Interrupted::Retry(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("management did not answer within {}s", limit.as_secs()),
        ))),
    }
}

fn lock(deployments: &Mutex<Deployments>) -> MutexGuard<'_, Deployments> {
    deployments.lock().unwrap_or_else(PoisonError::into_inner)
}

fn progress(deployment: &str, state: v1::DeploymentState, message: String) -> v1::DeploymentProgress {
    v1::DeploymentProgress { deployment_id: deployment.into(), state: state.into(), message }
}

fn bounded(mut message: String) -> String {
    message.truncate(message.floor_char_boundary(MAX_MESSAGE_BYTES));
    message
}

#[cfg(test)]
mod tests;
