//! Hosts on machines a [`Launcher`] starts. Each machine's runner fetches its host's release from core, and its JVM
//! registers with the machine credential core minted for the host.

mod command;

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
use tokio::time::Instant;

/// How long a launched JVM has to register by default. A cold remote start downloads its release first.
pub const READINESS: Duration = Duration::from_secs(120);

/// Why a JVM's registration is refused when it differs from what core launched on its host.
pub(crate) const LAUNCH_MISMATCH: &str = "the JVM differs from its host's launch";

/// Starts and stops the machines remote runners boot on.
#[tonic::async_trait]
pub trait Launcher: Send + Sync {
    /// Starts host `id`'s machine, whose runner presents `credential`. Core calls it at most once per host.
    /// # Errors
    /// Reports a machine that may not have started; core then releases the host.
    async fn launch(&self, id: &str, credential: &str, spec: &LaunchSpec) -> io::Result<()>;
    /// Stops host `id`'s machine. `true` only once no machine for `id` runs; `false` asks core to try again.
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
    /// How long a launched JVM has to register before its host fails.
    pub readiness: Duration,
    /// Where every launched JVM serves players, as when each machine shares this one's network.
    pub player_address: Option<IpAddr>,
}

impl RunnerConfig {
    /// Launches through `launcher` with the default readiness deadline.
    #[must_use]
    pub fn new(launcher: Arc<dyn Launcher>) -> Self {
        Self { launcher, readiness: READINESS, player_address: None }
    }
}

/// Runs each host on a machine its [`Launcher`] starts. The machine credentials and launch records it keeps in control
/// outlive core, so a JVM launched before core restarted re-attaches by them.
pub(crate) struct RunnerHost {
    environment: String,
    config: RunnerConfig,
    core: OnceLock<Attached>,
    /// The release whose archive each deployment's runners download, by deployment.
    releases: Mutex<BTreeMap<String, String>>,
    runners: Mutex<BTreeMap<String, Runner>>,
}

/// The control this host runs for, set once it serves.
struct Attached {
    control: Weak<Control>,
    issuer: Issuer,
    /// Where runners reach core.
    endpoint: String,
}

struct Runner {
    identity: JvmIdentity,
    credential: String,
    registration: Option<Registration>,
    /// Launched by this core, rather than before core restarted.
    launched: bool,
    /// When its readiness deadline started.
    since: Instant,
    /// Why the host can never provide its runtime.
    failure: Option<String>,
}

impl Runner {
    /// Whether this core owns the JVM: it launched it, or the JVM re-attached.
    fn attached(&self) -> bool {
        self.launched || self.registration.is_some()
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
        if let Some(connection) = self.connection() {
            return Progress::Ready(Box::new(connection));
        }
        if self.since.elapsed() >= readiness {
            let failure = format!("the JVM did not register within {} seconds", readiness.as_secs());
            self.failure = Some(failure.clone());
            return Progress::Failed(failure);
        }
        Progress::Pending
    }
}

impl RunnerHost {
    pub fn new(environment: &str, config: RunnerConfig) -> Self {
        Self {
            environment: environment.to_owned(),
            config,
            core: OnceLock::new(),
            releases: Mutex::default(),
            runners: Mutex::default(),
        }
    }

    /// Runs hosts for `control`, whose issuer mints their credentials, telling runners to reach core at `endpoint`.
    pub fn attach(&self, control: &Arc<Control>, issuer: Issuer, endpoint: String) {
        let _ = self.core.set(Attached { control: Arc::downgrade(control), issuer, endpoint });
    }

    /// Launches `deployment`'s JVMs from release `release`'s kept archive.
    pub fn add_release(&self, deployment: &str, release: &str) {
        lock(&self.releases).insert(deployment.to_owned(), release.to_owned());
    }

    fn core(&self) -> Result<(&Attached, Arc<Control>)> {
        let core = self.core.get().ok_or(Error::Unresolved("core is not serving yet"))?;
        Ok((core, core.control.upgrade().ok_or(Error::Unresolved("control stopped"))?))
    }

    fn runners(&self) -> Result<MutexGuard<'_, BTreeMap<String, Runner>>> {
        self.runners.lock().map_err(|_| Error::Unresolved("host poisoned"))
    }

    /// The progress of `id`'s runner, if this core knows it.
    fn progress(&self, id: &str, deployment: &str, app: &str, profile: &str) -> Result<Option<Progress>> {
        let mut runners = self.runners()?;
        let Some(runner) = runners.get_mut(id) else { return Ok(None) };
        let identity = &runner.identity;
        if identity.deployment != deployment || identity.app != app || identity.profile != profile {
            return Ok(Some(Progress::Failed("host binding changed".into())));
        }
        Ok(Some(runner.progress(self.config.readiness)))
    }

    /// Launches `id`'s machine unless its launch was recorded before core restarted, whose JVM may still re-attach.
    async fn start(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Progress> {
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
            let archive = lock(&self.releases).get(&deployment.deployment).cloned();
            let launch = Launch {
                deployment: deployment.deployment.clone(),
                release: archive.ok_or(Error::Invalid("core keeps no archive of the host's release"))?,
                app: app.to_owned(),
                profile: profile.to_owned(),
                process_id: uuid::Uuid::new_v4().to_string(),
                generation: 1,
                boot: None,
            };
            control.add_machine(id, MachineKind::Jvm)?;
            control.record_launch(id, launch.clone())?;
            (launch, true)
        };
        if launch.deployment != deployment.deployment || launch.app != app || launch.profile != profile {
            return Err(Error::Invalid("host binding changed"));
        }
        let credential = core.issuer.machine(MachineKind::Jvm, id);
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
            launched,
            since: Instant::now(),
            failure: None,
        };
        // A JVM that re-attached meanwhile keeps its runner.
        self.runners()?.entry(id.to_owned()).or_insert(runner);
        if launched {
            let spec = LaunchSpec {
                core_endpoint: core.endpoint.clone(),
                environment: self.environment.clone(),
                player_address: self.config.player_address,
                memory_mib: size.memory_mib,
            };
            if let Err(error) = self.config.launcher.launch(id, &credential, &spec).await {
                let failure = format!("launching the host's machine failed: {error}");
                if let Some(runner) = self.runners()?.get_mut(id) {
                    runner.failure = Some(failure.clone());
                }
                return Ok(Progress::Failed(failure));
            }
        }
        Ok(self.progress(id, &deployment.deployment, app, profile)?.unwrap_or(Progress::Pending))
    }
}

#[tonic::async_trait]
impl Host for RunnerHost {
    async fn ensure(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Progress> {
        if let Some(progress) = self.progress(id, &release.deployment.deployment, app, profile)? {
            return Ok(progress);
        }
        match self.start(id, release, app, profile).await {
            Err(Error::Invalid(reason)) => Ok(Progress::Failed(reason.into())),
            Err(Error::Stopped) => Ok(Progress::Failed("stopped".into())),
            progress => progress,
        }
    }

    async fn release(&self, id: &str) -> Result<bool> {
        let (_, control) = self.core()?;
        if let Some(runner) = self.runners()?.get_mut(id) {
            runner.failure.get_or_insert_with(|| "released".into());
        }
        if control.machine(id, MachineKind::Jvm) {
            control.revoke_machine(id, MachineKind::Jvm)?;
        }
        if control.launch_may_run(id)? && !self.config.launcher.release(id).await? {
            return Ok(false);
        }
        control.remove_launch(id)?;
        self.runners()?.remove(id);
        Ok(true)
    }

    fn stopped(&self, id: &str) -> bool {
        self.core().is_ok_and(|(_, control)| control.launch_may_run(id).is_ok_and(|may_run| !may_run))
    }

    fn unresolved(&self, id: &str) -> bool {
        let Ok((_, control)) = self.core() else { return true };
        let attached = self.runners().is_ok_and(|runners| runners.get(id).is_some_and(Runner::attached));
        !attached && control.launch(id).is_some()
    }

    fn unowned(&self) -> Result<BTreeSet<String>> {
        // Before core serves, only control opening asks, and every launch it records has a host row.
        let Ok((_, control)) = self.core() else { return Ok(BTreeSet::new()) };
        let mut hosts = control.launched_hosts()?;
        let runners = self.runners()?;
        hosts.retain(|id| !runners.get(id).is_some_and(Runner::attached));
        Ok(hosts)
    }

    fn register(&self, token: &str, registration: Registration) -> Result<()> {
        let mut runners = self.runners()?;
        let runner = runners.get_mut(&registration.identity.host).filter(|runner| runner.attached());
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
        let mut runners = self.runners()?;
        match runners.get_mut(&identity.host) {
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
                    launched: false,
                    since: Instant::now(),
                    failure: None,
                };
                runners.insert(host, runner);
                Ok(())
            }
        }
    }

    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        self.runners().ok()?.get(id)?.connection()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
