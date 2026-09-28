//! Hosts on machines a [`Launcher`] starts. Each machine's runner fetches its host's release from core, and its JVM
//! registers with the machine credential core minted for the host.

#[cfg(unix)]
mod command;

#[cfg(unix)]
pub use command::CommandLauncher;

use super::sync::Issuer;
use chunk_control::{
    Control, Error, Host, JvmIdentity, Launch, MachineKind, Progress, Registration, Release, Result, RuntimeConnection,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard, OnceLock, Weak},
    time::Duration,
};
use tokio::{
    sync::{OwnedMutexGuard, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

/// How long a launched JVM has to register by default, its machine's launch included. A cold remote start downloads its
/// release first.
pub const READINESS: Duration = Duration::from_secs(120);

/// How long a machine's release may take by default before core gives up on that attempt and tries again.
pub const RELEASE_TIMEOUT: Duration = Duration::from_secs(60);

/// Why a JVM's registration is refused when it differs from what core launched on its host.
pub(crate) const LAUNCH_MISMATCH: &str = "the JVM differs from its host's launch";

/// Why a released host provides nothing.
const RELEASED: &str = "the host was released";

/// Starts and stops the machines remote runners boot on.
///
/// A confirmed release is terminal for its host ID: no machine for that ID may start afterwards, even from a launch
/// still in flight or one an earlier core process started. The launcher owns that fence, since it owns machine
/// creation; the management launcher of P5 implements it there.
#[tonic::async_trait]
pub trait Launcher: Send + Sync {
    /// Starts host `id`'s machine, whose runner presents `credential`. Core calls it at most once per host, and cancels
    /// it once the host's readiness deadline passes or the host is released; either way it then releases the host,
    /// but only once this call returned. A cancelled call must stop its work and return once that ended.
    /// # Errors
    /// Reports a machine that may not have started; core then releases the host.
    async fn launch(&self, id: &str, credential: &str, spec: &LaunchSpec, cancel: &CancellationToken)
    -> io::Result<()>;
    /// Stops host `id`'s machine for good, including one whose launch failed or was cancelled. `true` only once no
    /// machine for `id` runs or can start, as for one never launched; `false` asks core to try again, as does a call
    /// core drops after its release timeout.
    /// # Errors
    /// Reports a stop that may not have happened; core tries again.
    async fn release(&self, id: &str) -> io::Result<bool>;
}

/// What a launcher needs to start a host's machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchSpec {
    /// Where the runner reaches core.
    pub core_endpoint: String,
    pub environment: String,
    /// Where the JVM serves players, when core knows it.
    pub player_address: Option<IpAddr>,
    /// The machine profile's memory.
    pub memory_mib: u32,
}

impl LaunchSpec {
    /// The runner's environment, with its machine credential `credential`.
    #[must_use]
    pub fn env(&self, credential: &str) -> Vec<(&'static str, String)> {
        let mut env = vec![
            ("CHUNK_CORE_ENDPOINT", self.core_endpoint.clone()),
            ("CHUNK_JVM_CREDENTIAL", credential.to_owned()),
            ("CHUNK_ENVIRONMENT_ID", self.environment.clone()),
        ];
        env.extend(self.player_address.map(|address| ("CHUNK_PLAYER_ADDRESS", address.to_string())));
        env
    }
}

pub struct RunnerConfig {
    pub launcher: Arc<dyn Launcher>,
    /// How long a host's machine has to launch and its JVM to register before the host fails.
    pub readiness: Duration,
    /// How long one release of a machine may take before core tries again.
    pub release_timeout: Duration,
    /// Where every launched JVM serves players, as when each machine shares this one's network.
    pub player_address: Option<IpAddr>,
}

impl RunnerConfig {
    /// Launches through `launcher` with the default readiness deadline and release timeout.
    #[must_use]
    pub fn new(launcher: Arc<dyn Launcher>) -> Self {
        Self { launcher, readiness: READINESS, release_timeout: RELEASE_TIMEOUT, player_address: None }
    }
}

/// Runs each host on a machine its [`Launcher`] starts. The machine credentials and launch records it keeps in control
/// outlive core, so a JVM launched before core restarted re-attaches by them. A host released once never launches again,
/// and its release cancels its launch and waits for that to end before the launcher releases it.
pub(crate) struct RunnerHost {
    environment: String,
    config: RunnerConfig,
    core: OnceLock<Attached>,
    hosts: Mutex<Hosts>,
    /// The turn each host's launch start or release holds, while one is held or awaited.
    turns: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// The control this host runs for, set once it serves.
struct Attached {
    control: Weak<Control>,
    issuer: Issuer,
    /// Where runners reach core.
    endpoint: String,
}

#[derive(Default)]
struct Hosts {
    runners: BTreeMap<String, Runner>,
    /// Hosts whose release began, which never launch again. Control's revoked machine records it durably too.
    released: BTreeSet<String>,
    /// Released hosts whose machine the launcher confirmed stopped, whether or not control could record it.
    stopped: BTreeSet<String>,
}

struct Runner {
    identity: JvmIdentity,
    credential: String,
    registration: Option<Registration>,
    /// Its machine's launch, when this core started one rather than one before core restarted.
    launching: Option<Launching>,
    /// When its readiness deadline started.
    since: Instant,
    /// Why the host can never provide its runtime.
    failure: Option<String>,
}

/// A machine's launch, which runs in its own task so that no caller dropping its wait stops it midway.
struct Launching {
    cancel: CancellationToken,
    ended: Ended,
}

/// How a launch ended, once it did: its failure, if any.
type Ended = watch::Receiver<Option<std::result::Result<(), String>>>;

impl Runner {
    /// Whether this core owns the JVM: it launched it, or the JVM re-attached.
    fn attached(&self) -> bool {
        self.launching.is_some() || self.registration.is_some()
    }

    fn connection(&self) -> Option<RuntimeConnection> {
        let registration = self.registration.as_ref().filter(|_| self.failure.is_none())?;
        Some(RuntimeConnection {
            token: self.credential.clone(),
            identity: self.identity.clone(),
            player_endpoint: registration.player_endpoint.clone(),
        })
    }

    fn progress(&mut self, readiness: Duration) -> Progress {
        if let Some(failure) = &self.failure {
            return Progress::Failed(failure.clone());
        }
        let ended = self.launching.as_ref().map(|launching| launching.ended.borrow().clone());
        if let Some(Some(Err(failure))) = ended {
            return self.fail(failure);
        }
        if let Some(connection) = self.connection() {
            return Progress::Ready(Box::new(connection));
        }
        if self.since.elapsed() >= readiness {
            if ended.is_some_and(|ended| ended.is_none()) {
                return self.fail(unfinished(readiness));
            }
            return self.fail(format!("the JVM did not register within {} seconds", readiness.as_secs()));
        }
        Progress::Pending
    }

    fn fail(&mut self, failure: String) -> Progress {
        Progress::Failed(self.failure.get_or_insert(failure).clone())
    }
}

impl RunnerHost {
    pub fn new(environment: &str, config: RunnerConfig) -> Self {
        Self {
            environment: environment.to_owned(),
            config,
            core: OnceLock::new(),
            hosts: Mutex::default(),
            turns: Mutex::default(),
        }
    }

    /// Runs hosts for `control`, whose issuer mints their credentials, telling runners to reach core at `endpoint`.
    pub fn attach(&self, control: &Arc<Control>, issuer: Issuer, endpoint: String) {
        let _ = self.core.set(Attached { control: Arc::downgrade(control), issuer, endpoint });
    }

    fn core(&self) -> Result<(&Attached, Arc<Control>)> {
        let core = self.core.get().ok_or(Error::Unresolved("core is not serving yet"))?;
        Ok((core, core.control.upgrade().ok_or(Error::Unresolved("control stopped"))?))
    }

    fn hosts(&self) -> Result<MutexGuard<'_, Hosts>> {
        self.hosts.lock().map_err(|_| Error::Unresolved("host poisoned"))
    }

    /// Waits for `id`'s turn to launch or release, which it holds until the guard drops.
    async fn turn(&self, id: &str) -> OwnedMutexGuard<()> {
        let turn = {
            let mut turns = lock(&self.turns);
            // Only the map holds an idle turn, so dropping it cannot split a holder from its waiters.
            turns.retain(|_, turn| Arc::strong_count(turn) > 1);
            turns.entry(id.to_owned()).or_default().clone()
        };
        turn.lock_owned().await
    }

    /// The progress of `id`'s runner, if this core knows it.
    fn progress(&self, id: &str, deployment: &str, app: &str, profile: &str) -> Result<Option<Progress>> {
        let mut hosts = self.hosts()?;
        if hosts.released.contains(id) {
            return Ok(Some(Progress::Failed(RELEASED.into())));
        }
        let Some(runner) = hosts.runners.get_mut(id) else { return Ok(None) };
        let identity = &runner.identity;
        if identity.deployment != deployment || identity.app != app || identity.profile != profile {
            return Ok(Some(Progress::Failed("host binding changed".into())));
        }
        Ok(Some(runner.progress(self.config.readiness)))
    }

    /// Starts launching `id`'s machine unless its launch was recorded before core restarted, whose JVM may still
    /// re-attach, or the host was released. Returns how a launch it started ends, and when its deadline passes. Holds
    /// `id`'s turn.
    fn start(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Option<(Ended, Instant)>> {
        let (core, control) = self.core()?;
        let deployment = &release.deployment;
        if deployment.environment != self.environment {
            return Err(Error::Invalid("release belongs to another environment"));
        }
        let artifact = release.apps.get(app).ok_or(Error::Invalid("unknown app"))?;
        let size = release.profiles.get(profile).ok_or(Error::Invalid("unknown profile"))?;
        let (launch, launched) = if let Some(launch) = control.launch(id) {
            (launch, false)
        } else {
            let launch = Launch {
                deployment: deployment.deployment.clone(),
                release: release.artifact_digest.clone(),
                app: app.to_owned(),
                profile: profile.to_owned(),
                process_id: uuid::Uuid::new_v4().to_string(),
                generation: 1,
                boot: None,
            };
            // Recorded before the machine exists, so every machine that may run has a launch to release.
            control.record_launch(id, launch.clone())?;
            (launch, true)
        };
        if launch.deployment != deployment.deployment || launch.app != app || launch.profile != profile {
            return Err(Error::Invalid("host binding changed"));
        }
        let credential = core.issuer.machine(MachineKind::Jvm, id);
        let (report, ended) = watch::channel(None);
        let cancel = CancellationToken::new();
        let runner = Runner {
            identity: JvmIdentity {
                host: id.to_owned(),
                process_id: launch.process_id,
                generation: launch.generation,
                deployment: launch.deployment,
                app: launch.app,
                profile: launch.profile,
                artifact_digest: artifact.sha256.clone(),
            },
            credential: credential.clone(),
            registration: None,
            launching: launched.then(|| Launching { cancel: cancel.clone(), ended: ended.clone() }),
            since: Instant::now(),
            failure: None,
        };
        let deadline = runner.since + self.config.readiness;
        {
            let mut hosts = self.hosts()?;
            // A release that began meanwhile has cancelled no launch yet.
            if hosts.released.contains(id) {
                return Ok(None);
            }
            // A JVM that re-attached meanwhile keeps its runner, and nothing launches.
            if hosts.runners.contains_key(id) {
                return Ok(None);
            }
            hosts.runners.insert(id.to_owned(), runner);
            if !launched {
                return Ok(None);
            }
        }
        let spec = LaunchSpec {
            core_endpoint: core.endpoint.clone(),
            environment: self.environment.clone(),
            player_address: self.config.player_address,
            memory_mib: size.memory_mib,
        };
        let (machines, id, readiness) = (self.config.launcher.clone(), id.to_owned(), self.config.readiness);
        tokio::spawn(async move {
            let launching = machines.launch(&id, &credential, &spec, &cancel);
            report.send_replace(Some(run_launch(launching, &cancel, deadline, readiness).await));
        });
        Ok(Some((ended, deadline)))
    }

    /// Marks `id` released, so it never launches again, and cancels its machine's launch if one runs.
    fn fence(&self, id: &str) -> Result<()> {
        let mut hosts = self.hosts()?;
        hosts.released.insert(id.to_owned());
        if let Some(runner) = hosts.runners.get_mut(id) {
            runner.failure.get_or_insert_with(|| RELEASED.into());
            if let Some(launching) = &runner.launching {
                launching.cancel.cancel();
            }
        }
        Ok(())
    }

    /// Stops `id`'s machine through the launcher unless it already confirmed that, or `control`, when given, records
    /// nothing that may run there. Waits first for the launch [`Self::fence`] cancelled to end, so none runs while the
    /// launcher releases. Needs no commit, so it works once the store stopped. Holds `id`'s turn.
    async fn stop_machine(&self, id: &str, control: Option<&Control>) -> Result<bool> {
        let (may_run, launching) = {
            let hosts = self.hosts()?;
            if hosts.stopped.contains(id) {
                return Ok(true);
            }
            let runner = hosts.runners.get(id);
            let launching =
                runner.and_then(|runner| runner.launching.as_ref()).map(|launching| launching.ended.clone());
            let may_run = runner.is_some() || control.is_none_or(|control| control.launch_may_run(id).unwrap_or(true));
            (may_run, launching)
        };
        if let Some(mut ended) = launching {
            // A closed channel means the launch's task, and so the launch, is gone.
            let waited = tokio::time::timeout(self.config.release_timeout, ended.wait_for(Option::is_some)).await;
            if waited.is_err() {
                let timeout = self.config.release_timeout.as_secs();
                tracing::warn!(host = id, timeout, "the machine's launch did not end in time; trying again");
                return Ok(false);
            }
        }
        if may_run {
            let Ok(released) =
                tokio::time::timeout(self.config.release_timeout, self.config.launcher.release(id)).await
            else {
                let timeout = self.config.release_timeout.as_secs();
                tracing::warn!(host = id, timeout, "the machine's release did not finish in time; trying again");
                return Ok(false);
            };
            if !released? {
                return Ok(false);
            }
        }
        let mut hosts = self.hosts()?;
        hosts.runners.remove(id);
        hosts.stopped.insert(id.to_owned());
        Ok(true)
    }

    /// Stops every machine that may run: each this core launched or that re-attached, and each whose launch control
    /// still records, without committing, as when control could not stop them. None launches again.
    /// # Errors
    /// Reports a machine whose stop is unconfirmed.
    pub async fn shutdown(&self) -> Result<()> {
        let control = self.core().ok().map(|(_, control)| control);
        let mut ids: BTreeSet<_> = self.hosts()?.runners.keys().cloned().collect();
        if let Some(control) = &control {
            ids.extend(control.launched_hosts()?);
        }
        let mut result = Ok(());
        for id in ids {
            self.fence(&id)?;
            let _turn = self.turn(&id).await;
            match self.stop_machine(&id, control.as_deref()).await {
                Ok(true) => {}
                Ok(false) => result = Err(Error::Unresolved("machine stop not confirmed")),
                Err(error) => result = Err(error),
            }
        }
        result
    }

    /// Stops the machines an earlier core launched for `hosts`, as before a fresh start drops their launch records.
    /// # Errors
    /// Reports a machine that may still run, whose records must then stay.
    pub async fn stop_recorded(&self, hosts: BTreeSet<String>) -> io::Result<()> {
        for id in hosts {
            let _turn = self.turn(&id).await;
            if !self.stop_machine(&id, None).await.map_err(io::Error::other)? {
                return Err(io::Error::other(format!("host {id}'s machine may still run")));
            }
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl Host for RunnerHost {
    async fn ensure(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Progress> {
        let deployment = &release.deployment.deployment;
        if let Some(progress) = self.progress(id, deployment, app, profile)? {
            return Ok(progress);
        }
        let started = {
            let _turn = self.turn(id).await;
            // A release or another launch may have taken the turn first.
            if let Some(progress) = self.progress(id, deployment, app, profile)? {
                return Ok(progress);
            }
            match self.start(id, release, app, profile) {
                Err(Error::Invalid(reason)) => return Ok(Progress::Failed(reason.into())),
                Err(Error::Stopped) => return Ok(Progress::Failed("stopped".into())),
                started => started?,
            }
        };
        // The launch runs on if this wait is dropped, and the wait holds no turn, so a release never waits for it.
        if let Some((mut ended, deadline)) = started {
            let _ = tokio::time::timeout_at(deadline, ended.wait_for(Option::is_some)).await;
            // Reported from the launch itself, since a release may have finished and control pruned the host since.
            if let Some(Err(failure)) = ended.borrow().clone() {
                return Ok(Progress::Failed(failure));
            }
        }
        Ok(self.progress(id, deployment, app, profile)?.unwrap_or(Progress::Pending))
    }

    async fn release(&self, id: &str) -> Result<bool> {
        let control = self.core().ok().map(|(_, control)| control);
        self.fence(id)?;
        let _turn = self.turn(id).await;
        let revoked =
            control.as_ref().map_or(Err(Error::Unresolved("control stopped")), |control| control.revoke_launch(id));
        // A revocation that didn't commit, as once the store stopped, still stops the machine.
        if !self.stop_machine(id, control.as_deref()).await? {
            return Ok(false);
        }
        revoked?;
        control.map_or(Ok(()), |control| control.remove_launch(id))?;
        Ok(true)
    }

    fn stopped(&self, id: &str) -> bool {
        self.hosts().is_ok_and(|hosts| hosts.stopped.contains(id))
            || self.core().is_ok_and(|(_, control)| control.launch_may_run(id).is_ok_and(|may_run| !may_run))
    }

    fn unresolved(&self, id: &str) -> bool {
        let Ok((_, control)) = self.core() else { return true };
        let Ok(hosts) = self.hosts() else { return true };
        let owned = hosts.runners.get(id).is_some_and(Runner::attached) || hosts.stopped.contains(id);
        !owned && control.launch(id).is_some()
    }

    fn unowned(&self) -> Result<BTreeSet<String>> {
        // Before core serves, only control opening asks, and every launch it records has a host row.
        let Ok((_, control)) = self.core() else { return Ok(BTreeSet::new()) };
        let mut ids = control.launched_hosts()?;
        let hosts = self.hosts()?;
        ids.retain(|id| !hosts.runners.get(id).is_some_and(Runner::attached) && !hosts.stopped.contains(id));
        Ok(ids)
    }

    fn prune(&self, retained: &BTreeSet<String>) -> Result<()> {
        // Control no longer ensures a host it forgot, and its revoked machine still fences it.
        let mut hosts = self.hosts()?;
        hosts.released.retain(|id| retained.contains(id));
        hosts.stopped.retain(|id| retained.contains(id));
        Ok(())
    }

    fn register(&self, token: &str, registration: Registration) -> Result<()> {
        let mut hosts = self.hosts()?;
        let runner = hosts.runners.get_mut(&registration.identity.host).filter(|runner| runner.attached());
        let runner = runner.ok_or(Error::Invalid("unknown process"))?;
        let secret = token.strip_prefix("Bearer ").unwrap_or_default();
        if !chunk_service::same_secret(secret, &runner.credential) {
            return Err(Error::Invalid("invalid process credential"));
        }
        if registration.identity != runner.identity {
            return Err(Error::Invalid(LAUNCH_MISMATCH));
        }
        if runner.failure.is_some() {
            return Err(Error::Stopped);
        }
        if runner.registration.as_ref().is_some_and(|previous| previous != &registration) {
            return Err(Error::Invalid("registration changed"));
        }
        runner.registration = Some(registration);
        Ok(())
    }

    fn adopt(&self, token: &str, registration: Registration) -> Result<()> {
        let (core, control) = self.core()?;
        let identity = registration.identity.clone();
        let launch = control.launch(&identity.host).ok_or(Error::Invalid("core launches nothing on this host"))?;
        let credential = core.issuer.machine(MachineKind::Jvm, &identity.host);
        if !control.machine(&identity.host, MachineKind::Jvm) || !chunk_service::same_secret(token, &credential) {
            return Err(Error::Invalid("invalid process credential"));
        }
        if launch.process_id != identity.process_id
            || launch.generation != identity.generation
            || launch.deployment != identity.deployment
            || launch.app != identity.app
            || launch.profile != identity.profile
        {
            return Err(Error::Invalid(LAUNCH_MISMATCH));
        }
        let mut hosts = self.hosts()?;
        if hosts.released.contains(&identity.host) {
            return Err(Error::Stopped);
        }
        match hosts.runners.get_mut(&identity.host) {
            Some(runner) if runner.identity != identity => Err(Error::Invalid(LAUNCH_MISMATCH)),
            Some(runner) if runner.failure.is_some() => Err(Error::Stopped),
            Some(runner) if runner.attached() => Err(Error::Invalid("process is not awaiting re-attachment")),
            Some(runner) => {
                runner.registration = Some(registration);
                Ok(())
            }
            None => {
                let host = identity.host.clone();
                let runner = Runner {
                    identity,
                    credential,
                    registration: Some(registration),
                    launching: None,
                    since: Instant::now(),
                    failure: None,
                };
                hosts.runners.insert(host, runner);
                Ok(())
            }
        }
    }

    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        self.hosts().ok()?.runners.get(id)?.connection()
    }
}

/// Runs `launching` until it ends, cancelling it once `deadline` passes, and returns why its host fails, if it does. A
/// launch that doesn't finish may still have started a machine, whose launch record stays for its release.
async fn run_launch(
    launching: impl Future<Output = io::Result<()>>,
    cancel: &CancellationToken,
    deadline: Instant,
    readiness: Duration,
) -> std::result::Result<(), String> {
    tokio::pin!(launching);
    let (launched, timed_out) = tokio::select! {
        biased;
        launched = &mut launching => (launched, false),
        () = tokio::time::sleep_until(deadline) => {
            cancel.cancel();
            (launching.await, true)
        }
    };
    match launched {
        _ if timed_out => Err(unfinished(readiness)),
        _ if cancel.is_cancelled() => Err(RELEASED.to_owned()),
        Ok(()) => Ok(()),
        Err(error) => Err(format!("launching the host's machine failed: {error}")),
    }
}

/// Why a launch that ran past the readiness deadline failed its host.
fn unfinished(readiness: Duration) -> String {
    format!("launching the host's machine did not finish within {readiness:?}")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
