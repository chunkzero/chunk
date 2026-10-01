//! Serving the deployments the management service asks for, through `EnvironmentService.Attach` and `ReportStatus`,
//! handing the backend's next due job to `SetWakeAlarm`, and reporting logs, metrics and usage.

mod activation;
mod alarm;
mod failed_auth;
mod idle;
mod launcher;
mod log_store;
mod metrics;
mod release;
mod retire;
mod secrets;
mod status;
mod telemetry;
mod usage;

use crate::{Core, CoreConfig, Gateway, GatewayConfig};
use activation::{Activation, Records};
use chunk_control::DrainPolicy;
use chunk_management::{Client, Code, v1};
pub(crate) use launcher::{Lease, ManagementLauncher};
pub(crate) use log_store::renewing;
use log_store::{LogStore, Renewal, Renewer};
use std::{
    collections::BTreeSet,
    io,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, MutexGuard, OnceLock, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use telemetry::Telemetry;
use tokio::{sync::watch, time::MissedTickBehavior};
use tokio_util::sync::CancellationToken;

const REATTACH: Duration = Duration::from_secs(5);
/// Management resends the desired state at least every 30 seconds, so a quieter stream is stalled.
const ATTACH_IDLE: Duration = Duration::from_secs(90);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Why a deployment failed, as reported, is cut to this many bytes.
const MAX_MESSAGE_BYTES: usize = 1024;
/// How long core waits for one signal of whether it may be suspended before it counts that signal as unknown.
const READ_WAIT: Duration = Duration::from_millis(500);
/// How long one observation of core's status may take, gateway address discovery included.
const OBSERVE_WAIT: Duration = Duration::from_secs(1);

pub struct ManagementConfig {
    /// The management service's base URL.
    pub url: String,
    /// The environment's bearer token.
    pub token: String,
    /// How long core stays idle before it reports that management may suspend it; unset, it never does. Positive, so
    /// work that came and went between two looks at it still counts.
    pub suspend_after: Option<Duration>,
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
    /// Set from sending an attach until its first desired state publishes its lease. Meanwhile management may have
    /// granted this core a lease it hasn't seen, so a report fenced under the published one may be this core's own doing.
    attaching: AtomicBool,
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
    /// The backend retirements in flight.
    retirements: retire::Retirements,
    reporter: status::Reporter,
    alarm: alarm::Alarm,
    idle: idle::Idle,
    /// Where edges reach the gateway.
    private_address: status::PrivateAddress,
    /// Renewed from each desired state of the core attach.
    log_store: Arc<LogStore>,
    /// Renews `log_store`'s credentials from an attach that takes no lease until the core attach first answers.
    renewal: Mutex<Option<Renewal>>,
    /// Ships core's log lines and usage, and outlives this attachment for a last shipment.
    telemetry: Arc<Telemetry>,
    /// The secrets the backend holds.
    secrets: Mutex<secrets::Versions>,
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
    /// How replaced deployments drain, from the latest desired state this process received.
    drain: DrainPolicy,
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
        registration: Registration,
        state: &Path,
        core: &'a Core,
        gateway: &'a OnceLock<Gateway>,
        gateway_config: Option<GatewayConfig>,
    ) -> Self {
        let (client, stopping) = (management.client(), CancellationToken::new());
        let telemetry =
            Telemetry::new(client.clone(), registration.instance_id.clone(), &crate::logs::LINES, lease.subscribe());
        Self {
            reporter: status::Reporter::new(client.clone(), stopping.clone()),
            alarm: alarm::Alarm::new(client.clone()),
            idle: idle::Idle::new(management.suspend_after),
            private_address: status::PrivateAddress::new(core.private_address(), &management.url),
            client,
            lease,
            attaching: AtomicBool::new(false),
            instance_id: registration.instance_id,
            log_store: registration.log_store,
            renewal: Mutex::new(registration.renewal),
            telemetry: Arc::new(telemetry),
            secrets: Mutex::new(registration.secrets),
            environment: registration.environment,
            releases: release::Store::new(state, core.archives().clone()),
            activation: state.join("managed.json"),
            core,
            gateway,
            gateway_config,
            deployments: Mutex::default(),
            retirements: retire::Retirements::default(),
            stopping,
        }
    }

    /// The token that tells this attachment core is shutting down.
    pub(crate) fn stopping(&self) -> CancellationToken {
        self.stopping.clone()
    }

    /// What renews the log store's credentials once this attachment ends.
    pub(crate) fn renewer(&self) -> Renewer {
        Renewer {
            client: self.client.clone(),
            instance_id: self.instance_id.clone(),
            environment: self.environment.clone(),
            log_store: self.log_store.clone(),
        }
    }

    /// What ships core's last log lines and usage once this attachment ends.
    pub(crate) fn telemetry(&self) -> Arc<Telemetry> {
        self.telemetry.clone()
    }

    /// Stops the renewal from the attach that took no lease, so only the core attach renews from here on.
    async fn take_over_renewal(&self) {
        let renewal = self.renewal.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(renewal) = renewal {
            renewal.stop().await;
        }
    }

    /// Attaches until management fences this core or it cannot serve, attaching again after other interruptions.
    /// Meanwhile, it retires the deployments it no longer serves, reports its status, metrics, logs and usage, and hands
    /// off its wake alarm.
    pub(crate) async fn run(&self) -> io::Error {
        if let Err(error) = self.recover() {
            return error;
        }
        if let Err(error) = release::sweep(&self.releases).await {
            tracing::warn!(%error, "abandoned release downloads not removed");
        }
        let started = Instant::now();
        let restored = match self.core.control().and_then(|control| control.release_ids().map_err(io::Error::other)) {
            Ok(retained) => release::restore(&self.releases, retained).await,
            Err(error) => Err(error),
        };
        match restored {
            Ok(()) => tracing::info!(elapsed_ms = millis(started), "kept release archives restored"),
            Err(error) => tracing::warn!(%error, "kept release archives not restored"),
        }
        tokio::select! {
            error = self.follow() => error,
            never = self.reclaim() => match never {},
            never = self.hand_off_alarms() => match never {},
            never = self.report_failed_auth() => match never {},
            never = self.report_metrics() => match never {},
            never = self.count_usage() => match never {},
            never = self.telemetry.keep_shipping() => match never {},
            error = self.reporter.keep_reporting(|| self.current(), |lease| self.superseded(lease)) => self.fenced(error),
        }
    }

    /// Counts awake time and the players online every [`usage::TICK`] while core holds the environment.
    async fn count_usage(&self) -> std::convert::Infallible {
        let mut tick = tokio::time::interval(usage::TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            self.telemetry.tick(self.online_players().unwrap_or(0));
        }
    }

    /// Hands off each new wake alarm of the backend under the lease core holds, unless core is stopping.
    async fn hand_off_alarms(&self) -> ! {
        match (self.core.backend(), self.core.epoch()) {
            (Some(backend), Ok(epoch)) => self.alarm.keep_handing_off(&backend, epoch, || self.held()).await,
            _ => std::future::pending().await,
        }
    }

    /// The lease of the latest attach, unless core is stopping or was superseded.
    fn held(&self) -> Option<u64> {
        match *self.lease.borrow() {
            Lease::Held(lease) if !self.stopping.is_cancelled() => Some(lease),
            _ => None,
        }
    }

    /// Whether management fencing `lease` means another core superseded this one: `lease` is the one core holds, and
    /// no attach since may have replaced it.
    fn superseded(&self, lease: u64) -> bool {
        !self.attaching.load(Ordering::SeqCst) && *self.lease.borrow() == Lease::Held(lease)
    }

    /// Publishes that another core superseded this one, which must stop serving.
    fn fenced(&self, error: io::Error) -> io::Error {
        self.lease.send_replace(Lease::Superseded);
        error
    }

    /// Protects an activation an earlier run recorded until management accepts it, and restores the deployments it
    /// was to stop, if control made it current.
    fn recover(&self) -> io::Result<()> {
        let current = self.core.control()?.current_release().map_err(io::Error::other)?;
        let activations = activation::recorded::<Activation>(&self.activation)?;
        let unacknowledged = [activations.pending, activations.committed]
            .into_iter()
            .flatten()
            .find(|recorded| current.as_ref() == Some(&recorded.activated));
        Records { committed: unacknowledged.as_ref(), pending: None }.store(&self.activation)?;
        lock(&self.deployments).unacknowledged = unacknowledged;
        Ok(())
    }

    async fn follow(&self) -> io::Error {
        loop {
            match self.attach().await {
                Ok(()) => tracing::warn!("management ended the attach"),
                Err(Interrupted::Retry(error)) => tracing::warn!(%error, "management attach interrupted"),
                Err(Interrupted::Fatal(error)) => return error,
                Err(Interrupted::Fenced(error)) => return self.fenced(error),
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
        self.attaching.store(true, Ordering::SeqCst);
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
                    check(&desired, &self.environment)?;
                    self.take_over_renewal().await;
                    self.log_store.renew(desired.log_store.as_ref());
                    self.install_secrets(&desired);
                    self.lease.send_replace(Lease::Held(desired.lease));
                    self.attaching.store(false, Ordering::SeqCst);
                    {
                        let mut deployments = lock(&self.deployments);
                        deployments.revision = desired.revision;
                        deployments.desired = Some(desired.deployment_id.clone());
                        deployments.drain = retire::drain_policy(&desired);
                    }
                    self.apply_drain_policy(&desired);
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

    /// Installs `desired`'s secrets once their names or versions differ from those the backend holds. Actions that
    /// start afterwards read them.
    fn install_secrets(&self, desired: &v1::AttachResponse) {
        let versions = secrets::versions(desired);
        let mut installed = self.secrets.lock().unwrap_or_else(PoisonError::into_inner);
        if *installed != versions {
            self.core.set_secrets(secrets::decode(desired));
            tracing::info!(secrets = versions.len(), "secrets updated");
            *installed = versions;
        }
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
        } else if self.resumes(&desired).await {
            let started = Instant::now();
            self.route(deployment).await.map_err(Interrupted::Fatal)?;
            tracing::info!(deployment, route_ms = millis(started), "current deployment resumed");
            (v1::DeploymentState::Active, String::new())
        } else {
            let started = Instant::now();
            self.report(&desired, Some(progress(deployment, v1::DeploymentState::InProgress, String::new()))).await?;
            match self.deploy(&desired, &cancel).await {
                Ok(false) => return Ok(()),
                Ok(true) => {
                    let (deploy_ms, routing) = (millis(started), Instant::now());
                    self.route(deployment).await.map_err(Interrupted::Fatal)?;
                    tracing::info!(deployment, deploy_ms, route_ms = millis(routing), "deployment activated");
                    (v1::DeploymentState::Active, String::new())
                }
                Err(error) => {
                    tracing::warn!(%error, deployment, "deployment rejected; the previous one keeps serving");
                    let message = crate::logs::escaped(MAX_MESSAGE_BYTES, &error);
                    lock(&self.deployments).rejected = Some((deployment.into(), message.clone()));
                    (v1::DeploymentState::Failed, message)
                }
            }
        };
        self.report(&desired, Some(progress(deployment, state, message))).await
    }

    /// Whether `desired`'s deployment is control's current release, resident in the backend, with its release's archive
    /// kept as `desired` names it. This core checked that release when it activated it, so it serves it again unchecked.
    async fn resumes(&self, desired: &v1::AttachResponse) -> bool {
        let Some(artifact) = &desired.release else { return false };
        let current = self.core.control().and_then(|control| control.current_release().map_err(io::Error::other));
        if current.ok().flatten().as_deref() != Some(desired.deployment_id.as_str())
            || !release::kept(&self.releases, artifact)
        {
            return false;
        }
        let (Some(backend), Ok(id)) = (self.core.backend(), chunk_js::DeploymentId::new(&desired.deployment_id)) else {
            return false;
        };
        backend.check_deployment(id).await.is_ok()
    }

    /// Loads the deployment's release, makes it resident in the backend and waits until it is ready, and, unless
    /// `cancel` superseded it by then, makes it control's current release. Returns whether it did.
    async fn deploy(&self, desired: &v1::AttachResponse, cancel: &CancellationToken) -> io::Result<bool> {
        let artifact = desired.release.as_ref().ok_or_else(|| io::Error::other("the deployment names no release"))?;
        let Some(loaded) = release::load(&self.client, &self.releases, artifact, cancel).await? else {
            return Ok(false);
        };
        let deployment = &desired.deployment_id;
        if cancel.is_cancelled() {
            return Ok(false);
        }
        if !self.core.deploy_when_free(loaded.bundle(deployment), cancel).await? {
            return Ok(false);
        }
        let resident = match self.core.backend() {
            Some(backend) => backend.deployments().await.map_err(io::Error::other)?,
            None => return Err(io::Error::other("backend is not running")),
        };
        // Nothing awaits between this check and switching traffic, so the attach loop cannot supersede it meanwhile.
        if cancel.is_cancelled() {
            return Ok(false);
        }
        let control = self.core.control()?;
        let current = control.current_release().map_err(io::Error::other)?;
        let mut deployments = lock(&self.deployments);
        // Recorded beside the committed record first, so a crash once control has activated it still protects the
        // predecessor, and one before keeps what the record held. Activating the unaccepted deployment again keeps its
        // predecessor.
        let predecessor = match &deployments.unacknowledged {
            Some(pending) if pending.activated == *deployment => pending.predecessor.clone(),
            _ => current.clone().filter(|current| current != deployment),
        };
        let pending = Activation { predecessor, activated: deployment.clone() };
        let stop = if desired.stop_previous {
            resident.iter().map(|id| id.as_str().to_owned()).collect()
        } else {
            BTreeSet::new()
        };
        let earlier = deployments.unacknowledged.clone();
        Records { committed: earlier.as_ref(), pending: Some(&pending) }.store(&self.activation)?;
        let release = loaded.control(&self.environment, deployment);
        if let Err(error) = self.core.activate_stopping(release, deployments.drain, &stop) {
            _ = Records { committed: earlier.as_ref(), pending: None }.store(&self.activation);
            return Err(error);
        }
        // Should these writes fail, recovery promotes the pending records of an activation control committed.
        let activation = Records { committed: Some(&pending), pending: None }.store(&self.activation);
        if let Err(error) = activation {
            tracing::warn!(%error, "activation not recorded as committed");
        }
        deployments.unacknowledged = Some(pending);
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
        self.telemetry.lines().serving(deployment);
        Ok(())
    }

    /// Reports status under `desired`'s lease unless core is stopping. Once management accepts an activation, the
    /// deployment served before it may retire.
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
        if !self.reporter.send(observed, deployment).await? {
            return Ok(());
        }
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
        let lease = self.held()?;
        let revision = lock(&self.deployments).revision;
        Some(self.observe(lease, revision).await)
    }

    /// Core's status under `lease` and `revision`. An observation that takes longer than [`OBSERVE_WAIT`] says core
    /// may not be suspended, which starts the grace period over, and repeats the gateway addresses management last
    /// accepted rather than any it hasn't verified.
    async fn observe(&self, lease: u64, revision: u64) -> status::Observed {
        if let Ok(observed) = tokio::time::timeout(OBSERVE_WAIT, self.observe_now(lease, revision)).await {
            return observed;
        }
        tracing::debug!("core's status took too long to observe; it isn't ready to suspend");
        self.idle.restart(revision);
        status::Observed {
            lease,
            revision,
            gateway_addresses: self.reporter.accepted_addresses(),
            online_players: 0,
            logins: self.logins(),
            ready_to_suspend: false,
            pings: self.pings(),
        }
    }

    async fn observe_now(&self, lease: u64, revision: u64) -> status::Observed {
        let address = async {
            let gateway = self.gateway.get()?;
            self.private_address.gateway(gateway.address()).await
        };
        let (address, ready_to_suspend) = tokio::join!(address, self.ready_to_suspend(revision));
        let gateway_addresses = address.as_ref().map(SocketAddr::to_string).into_iter().collect();
        let online_players = self.online_players().unwrap_or_else(|error| {
            tracing::warn!(%error, "online players unknown");
            0
        });
        status::Observed {
            lease,
            revision,
            gateway_addresses,
            online_players,
            logins: self.logins(),
            ready_to_suspend,
            pings: self.pings(),
        }
    }

    fn online_players(&self) -> io::Result<u32> {
        let online = self.core.control()?.online_players().map_err(io::Error::other)?;
        Ok(u32::try_from(online).unwrap_or(u32::MAX))
    }

    fn logins(&self) -> u64 {
        self.core.control().map_or(0, |control| control.logins())
    }

    /// The status the gateway last answered for each hostname.
    fn pings(&self) -> Vec<v1::PingStatus> {
        let Some(gateway) = self.gateway.get() else { return Vec::new() };
        let pings = gateway.reports().pings().into_iter();
        pings.map(|(hostname, status_json)| v1::PingStatus { hostname, status_json }).collect()
    }

    /// Whether management may suspend core under desired `revision`, as the proto's contract has it: no players remain,
    /// the log is flushed, the wake alarm is handed off and logs and usage are shipped, with no job due within the grace
    /// period, and nothing was active for the grace period. Active means backend work running, starting or finishing
    /// (actions, hooks, commands and jobs), a gateway that reports connections or that core can't hear from, an open
    /// claim or a launching host in control, a claimed job, or a deployment loading or not yet accepted. What can't be
    /// read within [`READ_WAIT`] counts as active.
    ///
    /// Queries, mutations and operator calls aren't counted: suspending stops or pauses the environment gracefully,
    /// every commit is durable before it's acknowledged, and a call a suspend cuts off fails as it would in a crash and
    /// is retried.
    async fn ready_to_suspend(&self, revision: u64) -> bool {
        if !self.idle.sleeps() {
            return false;
        }
        let backend = self.core.backend();
        let handoff = match &backend {
            Some(backend) => read_within(backend.wake_handoff()).await,
            None => None,
        };
        let work = backend.as_ref().map(|backend| backend.activity().observe());
        let in_use = self.core.control().and_then(|control| control.in_use().map_err(io::Error::other));
        let deploying = {
            let deployments = lock(&self.deployments);
            deployments.loading.is_some() || deployments.unacknowledged.is_some()
        };
        let active = deploying
            || work.is_none_or(|work| work.in_flight > 0)
            || self.core.gateways_active()
            || !matches!(in_use, Ok(false))
            || handoff.as_ref().is_none_or(|handoff| handoff.running > 0);
        let handed = match (self.core.epoch(), &handoff) {
            (Ok(epoch), Some(handoff)) => self.alarm.settled(epoch, handoff),
            _ => false,
        };
        let observed = idle::Observation {
            active,
            changes: work.map_or(0, |work| work.changes),
            settled: handed && self.core.flushed() && self.telemetry.settled(),
            due_at: handoff.and_then(|handoff| handoff.due_at),
        };
        self.idle.ready(revision, &observed)
    }
}

/// This run of core as management knows it before core starts.
pub(crate) struct Registration {
    /// Unique to this run of the process.
    instance_id: String,
    environment: String,
    log_store: Arc<LogStore>,
    /// Renews `log_store`'s credentials from the attach that took no lease, until the core attach takes over.
    renewal: Option<Renewal>,
    /// The secrets core starts with.
    secrets: secrets::Versions,
}

impl Registration {
    /// Reads the first desired state from an attach that claims no lease, since core must know where its log replicates
    /// before it opens the log, and only then knows the epoch a core attach carries. Attaches again until management
    /// answers, or returns `None` once `stop` is cancelled. Object storage management grants replaces `core`'s
    /// replication, and that attach keeps renewing its credentials while core restores and starts. It also names the
    /// environment, whose variables deployments read, grants the secrets core starts with and, for a fork, names the log
    /// core forks from.
    /// # Errors
    /// Reports a desired state for another environment, or an invalid log store or restore.
    pub(crate) async fn attach(
        management: &ManagementConfig,
        core: &mut CoreConfig,
        stop: &CancellationToken,
    ) -> io::Result<Option<Self>> {
        let environment = core.environment.clone();
        let instance_id = uuid::Uuid::new_v4().to_string();
        let client = management.client();
        let request = log_store::request(&instance_id);
        let attach = || async {
            let mut stream = deadline(REQUEST_TIMEOUT, client.attach(&request)).await?;
            match deadline(REQUEST_TIMEOUT, stream.message()).await? {
                Some(desired) => check(&desired, &environment).map(|()| (stream, desired)),
                None => Err(Interrupted::Retry(io::Error::other("management ended the attach"))),
            }
        };
        let (stream, desired) = loop {
            let error = tokio::select! {
                () = stop.cancelled() => return Ok(None),
                attached = attach() => match attached {
                    Ok(attached) => break attached,
                    Err(Interrupted::Fatal(error)) => return Err(error),
                    Err(Interrupted::Retry(error) | Interrupted::Fenced(error)) => error,
                },
            };
            tracing::warn!(%error, "management attach failed; core waits for it before opening its log");
            tokio::select! {
                () = stop.cancelled() => return Ok(None),
                () = tokio::time::sleep(REATTACH) => {}
            }
        };
        let (log_store, replication) = LogStore::open(desired.log_store.as_ref())?;
        if replication.is_some() {
            core.replication = replication;
        }
        if let Some(restore) = &desired.restore {
            core.fork = Some(log_store::fork_source(restore)?);
            tracing::info!(
                source = restore.source_environment_id,
                snapshot = restore.snapshot_id,
                "a fork; its first start forks the source's log"
            );
        }
        core.environment_name = Some(desired.environment_name.clone()).filter(|name| !name.is_empty());
        core.secrets = secrets::decode(&desired);
        let secrets = secrets::versions(&desired);
        let log_store = Arc::new(log_store);
        let renewer = Renewer {
            client,
            instance_id: instance_id.clone(),
            environment: environment.clone(),
            log_store: log_store.clone(),
        };
        let renewal = renewer.start(Some(stream));
        Ok(Some(Self { instance_id, environment, log_store, renewal, secrets }))
    }

    /// A run that replicates nothing management grants.
    #[cfg(test)]
    pub(crate) fn local(environment: &str) -> Self {
        let (log_store, _) = LogStore::open(None).expect("no grant");
        Self {
            instance_id: uuid::Uuid::new_v4().to_string(),
            environment: environment.into(),
            log_store: Arc::new(log_store),
            renewal: None,
            secrets: Vec::new(),
        }
    }
}

fn check(desired: &v1::AttachResponse, environment: &str) -> Result<(), Interrupted> {
    if desired.environment_id == environment {
        return Ok(());
    }
    Err(Interrupted::Fatal(io::Error::other(format!(
        "management attached this core to environment {:?}, not {environment:?}",
        desired.environment_id
    ))))
}

/// What `read` answers within [`READ_WAIT`], or nothing once it fails or takes longer.
async fn read_within<T, E>(read: impl std::future::Future<Output = Result<T, E>>) -> Option<T> {
    tokio::time::timeout(READ_WAIT, read).await.ok()?.ok()
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

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn lock(deployments: &Mutex<Deployments>) -> MutexGuard<'_, Deployments> {
    deployments.lock().unwrap_or_else(PoisonError::into_inner)
}

fn progress(deployment: &str, state: v1::DeploymentState, message: String) -> v1::DeploymentProgress {
    v1::DeploymentProgress { deployment_id: deployment.into(), state: state.into(), message }
}

#[cfg(test)]
mod tests;
