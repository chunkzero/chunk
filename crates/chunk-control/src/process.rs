use crate::{Error, Host, JvmIdentity, ProcessHostConfig, Progress, Registration, Release, Result, RuntimeConnection};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, TryLockError},
    io::{self, Write},
    process::Stdio,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    process::{Child, Command},
    time::{Instant, sleep},
};
use tokio_util::sync::CancellationToken;

pub struct ProcessHost {
    config: ProcessHostConfig,
    endpoint: OnceLock<String>,
    processes: Mutex<Processes>,
}
#[derive(Default)]
struct Processes {
    running: BTreeMap<String, Arc<Process>>,
    failed: BTreeSet<String>,
    /// Set by shutdown, which then stops the running processes; no launch may start after it.
    stopping: bool,
}
struct Process {
    identity: JvmIdentity,
    token: String,
    registration: Mutex<Option<Registration>>,
    stop: CancellationToken,
    stopped: AtomicBool,
    /// Re-attached after control restarted, so no child handle can confirm its exit.
    adopted: bool,
    launched: Instant,
}
impl Process {
    fn connection(&self) -> Option<RuntimeConnection> {
        let registration = self.registration.lock().ok()?.clone()?;
        Some(RuntimeConnection {
            identity: self.identity.clone(),
            token: self.token.clone(),
            player_endpoint: registration.player_endpoint,
        })
    }
}
impl Drop for ProcessHost {
    fn drop(&mut self) {
        if let Ok(processes) = self.processes.lock() {
            for process in processes.running.values() {
                process.stop.cancel();
            }
        }
    }
}
impl ProcessHost {
    #[must_use]
    pub fn new(config: ProcessHostConfig) -> Self {
        Self { config, endpoint: OnceLock::new(), processes: Mutex::default() }
    }
    fn path(&self, id: &str, extension: &str) -> Result<std::path::PathBuf> {
        uuid::Uuid::parse_str(id).map_err(|_| Error::Invalid("invalid host ID"))?;
        Ok(self.config.directory.join(id).with_extension(extension))
    }
    fn process(&self, id: &str) -> Result<Option<Arc<Process>>> {
        Ok(self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?.running.get(id).cloned())
    }
    /// The process running `id`, launching it if `id` never launched. `None` while a launch this host does not own
    /// may still run.
    fn launch(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Option<Arc<Process>>> {
        let mut processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
        if let Some(process) = processes.running.get(id) {
            if process.identity.deployment != release.deployment.deployment
                || process.identity.app != app
                || process.identity.profile != profile
            {
                return Err(Error::Invalid("host binding changed"));
            }
            return Ok(Some(process.clone()));
        }
        if processes.failed.contains(id) || self.path(id, "exit")?.try_exists()? {
            return Err(Error::Stopped);
        }
        if self.path(id, "launch")?.try_exists()? {
            return Ok(None);
        }
        if processes.stopping {
            return Err(Error::Stopped);
        }
        match self.start(id, release, app, profile) {
            Ok(process) => {
                processes.running.insert(id.into(), process.clone());
                Ok(Some(process))
            }
            Err(error) => {
                // No JVM started, and the launch lock excludes another attempt for this ID.
                processes.failed.insert(id.into());
                if let Err(persist) = self.record_exit(id, b"launch failed") {
                    tracing::error!(%persist, host = id, "cannot persist failed JVM launch");
                }
                Err(error)
            }
        }
    }
    fn record_exit(&self, id: &str, reason: &[u8]) -> Result<()> {
        std::fs::create_dir_all(&self.config.directory)?;
        let mut exit = chunk_service::private_file(&self.path(id, "exit")?)?;
        exit.write_all(reason)?;
        exit.sync_all()?;
        Ok(())
    }
    fn start(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Arc<Process>> {
        let endpoint = self.endpoint.get().ok_or(Error::Unresolved("control not listening"))?;
        let deployment = &release.deployment;
        let artifact = release.apps.get(app).ok_or(Error::Invalid("unknown app"))?;
        // Placement already bound the profile to the app's session or one of its declared destinations.
        let size = release.profiles.get(profile).ok_or(Error::Invalid("unknown profile"))?;
        if self.config.environment != deployment.environment {
            return Err(Error::Invalid("release belongs to another environment"));
        }
        let directory = self.config.releases.join(&release.release_id);
        let root = directory.canonicalize()?;
        let jar = directory.join(&artifact.jar).canonicalize()?;
        let bytes = std::fs::read(&jar)?;
        if !jar.starts_with(&root) || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(Error::Invalid("app artifact digest mismatch"));
        }
        classpath::verify(&root, &jar, &bytes)?;
        let assets = release.assets.materialize(&self.config.assets, app)?;
        std::fs::create_dir_all(&self.config.directory)?;
        let log_path = self.path(id, "jvm.log")?;
        let exit = self.path(id, "exit")?;
        let pid_path = self.path(id, "pid")?;
        let process = Arc::new(Process {
            identity: JvmIdentity {
                host: id.into(),
                process_id: uuid::Uuid::new_v4().to_string(),
                generation: 1,
                deployment: deployment.deployment.clone(),
                app: app.into(),
                profile: profile.into(),
                artifact_digest: artifact.sha256.clone(),
            },
            token: format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple()),
            registration: Mutex::default(),
            stop: CancellationToken::new(),
            stopped: AtomicBool::new(false),
            adopted: false,
            launched: Instant::now(),
        });
        // The launch marker records the process's identity and credential digest before the JVM exists, so the JVM can
        // re-attach after a restart. The JVM inherits the marker's lock, and control's handle closes once the spawn
        // returns, so only the JVM holds the lock.
        let marker = self.record_launch(id, &LaunchRecord::of(&process.identity, &process.token, endpoint))?;
        let (gate, mut open) = io::pipe()?;
        let child = (|| {
            let log = chunk_service::private_file(&log_path)?;
            let mut command = Command::new("/bin/sh");
            chunk_service::withhold_platform_env(command.as_std_mut());
            command
                .args(["-c", GATE, "sh"])
                .arg(&self.config.java)
                .arg(format!("-Xmx{}m", size.memory_mib))
                .arg("-jar")
                .arg(&jar)
                .env("CHUNK_PROCESS_TOKEN", &process.token)
                .env("CHUNK_ENVIRONMENT", &deployment.environment)
                .env("CHUNK_DEPLOYMENT", &deployment.deployment)
                .env("CHUNK_CORE_ENDPOINT", endpoint)
                // The endpoint's older name, for runtime JARs that predate `CHUNK_CORE_ENDPOINT`.
                .env("CHUNK_CONTROL_ENDPOINT", endpoint)
                .env("CHUNK_INSTANCE_ID", id)
                .env("CHUNK_PROCESS_ID", &process.identity.process_id)
                .env("CHUNK_PROCESS_GENERATION", "1")
                .env("CHUNK_MACHINE_PROFILE", profile)
                .env("CHUNK_ARTIFACT_DIGEST", &artifact.sha256)
                .env("CHUNK_APP_ID", app)
                .env("CHUNK_ASSETS", &assets)
                .stdin(Stdio::from(gate))
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .kill_on_drop(true);
            if let Some(address) = self.config.private_address {
                command.env("CHUNK_PLAYER_ADDRESS", address.to_string());
            }
            match &self.config.environment_name {
                Some(name) => command.env("CHUNK_ENVIRONMENT_NAME", name),
                None => command.env_remove("CHUNK_ENVIRONMENT_NAME"),
            };
            inherit_lock(&mut command, marker)?;
            command.spawn()
        })();
        let mut child = child?;
        // After control restarts, the JVM's recorded PID is the only way to kill it, so the JVM starts only once its PID
        // is recorded. Exec keeps the gate's PID and start time.
        let recorded = child
            .id()
            .ok_or(Error::Unresolved("the JVM exited before it started"))
            .and_then(|pid| pid::record(&pid_path, pid))
            .and_then(|()| Ok(open.write_all(b"\n")?));
        if let Err(error) = recorded {
            let _ = child.start_kill();
            return Err(error);
        }
        drop(open);
        let owned = process.clone();
        tokio::spawn(async move {
            match own_child(child, &owned).await {
                Ok(()) => {
                    if let Err(error) = std::fs::write(exit, b"stopped") {
                        tracing::error!(%error, "cannot persist JVM exit");
                    }
                    owned.stopped.store(true, Ordering::Release);
                }
                Err(error) => tracing::error!(%error, "JVM exit is unconfirmed"),
            }
        });
        Ok(process)
    }
    /// Host IDs with a record of `extension`.
    fn recorded(&self, extension: &str) -> Result<BTreeSet<String>> {
        let entries = match std::fs::read_dir(&self.config.directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeSet::new()),
            Err(error) => return Err(error.into()),
        };
        let mut ids = BTreeSet::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|found| found == extension)
                && let Some(id) = path.file_stem().and_then(|stem| stem.to_str())
                && uuid::Uuid::parse_str(id).is_ok()
            {
                ids.insert(id.into());
            }
        }
        Ok(ids)
    }
    /// Publishes `id`'s launch marker atomically and returns an exclusive lock on it. Dropping the handle keeps any
    /// lock a spawned JVM inherited; unlocking it would release the JVM's lock too.
    fn record_launch(&self, id: &str, record: &LaunchRecord) -> Result<File> {
        let (marker, staged) = (self.path(id, "launch")?, self.path(id, "launch.staged")?);
        match std::fs::remove_file(&staged) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let mut file = chunk_service::private_file(&staged)?;
        file.write_all(&serde_json::to_vec(record)?)?;
        file.sync_all()?;
        let lock = File::open(&staged)?;
        lock.try_lock().map_err(io::Error::from)?;
        std::fs::rename(staged, marker)?;
        File::open(&self.config.directory)?.sync_all()?;
        Ok(lock)
    }
    fn launch_record(&self, id: &str) -> Option<LaunchRecord> {
        serde_json::from_slice(&std::fs::read(self.path(id, "launch").ok()?).ok()?).ok()
    }
    /// The control endpoints the JVMs of unowned launches that may still run were given, with `None` for a launch
    /// whose record names none. Each such JVM re-attaches only at its endpoint.
    /// # Errors
    /// Reports unreadable launch markers.
    pub fn unowned_endpoints(&self) -> Result<BTreeSet<Option<String>>> {
        let records = self.unowned()?.into_iter().map(|id| self.launch_record(&id));
        Ok(records.map(|record| record.map(|record| record.control_endpoint).filter(|e| !e.is_empty())).collect())
    }
    /// Whether `id`'s launch marker proves its JVM exited, recording the exit when it does. A JVM holds its marker's
    /// lock until it exits, and control releases its own once the spawn returns or control stops, so a free lock
    /// means no JVM runs. Anything else leaves the exit unconfirmed.
    fn confirm_exit(&self, id: &str) -> bool {
        let Ok(marker) = self.path(id, "launch").and_then(|path| Ok(File::open(path)?)) else {
            return false;
        };
        match marker.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return false,
            Err(TryLockError::Error(error)) => {
                tracing::debug!(%error, host = id, "cannot tell whether an unowned JVM runs");
                return false;
            }
        }
        if let Err(error) = self.record_exit(id, b"exited while unowned") {
            tracing::error!(%error, host = id, "cannot persist confirmed JVM exit");
        }
        true
    }
    /// Whether `id`'s JVM is confirmed stopped within `wait`.
    async fn exits(&self, id: &str, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        while !self.stopped(id) {
            if Instant::now() >= deadline {
                return false;
            }
            sleep(Duration::from_millis(25)).await;
        }
        true
    }
    /// Requires a player endpoint with a port on loopback or on this machine's private address.
    fn validate_endpoint(&self, registration: &Registration) -> Result<()> {
        let endpoint = &registration.player_endpoint;
        let address: std::net::SocketAddr = endpoint
            .strip_prefix("http://")
            .unwrap_or(endpoint)
            .parse()
            .map_err(|_| Error::Invalid("process endpoint"))?;
        let ip = address.ip().to_canonical();
        let local = ip.is_loopback() || self.config.private_address.is_some_and(|private| private.to_canonical() == ip);
        if !local || address.port() == 0 {
            return Err(Error::Invalid("process requires loopback or this machine's private address"));
        }
        Ok(())
    }
    /// Stops all owned JVMs, including launches awaiting readiness, and launches no more.
    /// # Errors
    /// Reports unconfirmed process exits, including those of launches this host does not own.
    pub async fn shutdown(&self) -> Result<()> {
        // Read before the running processes: adoption moves a launch from unowned to running, so it can't escape both.
        let unowned = self.unowned()?;
        let ids: Vec<_> = {
            let mut processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
            processes.stopping = true;
            processes.running.keys().cloned().collect()
        };
        let mut result = Ok(());
        for id in ids {
            match self.release(&id).await {
                Ok(true) => {}
                Ok(false) => result = Err(Error::Unresolved("JVM shutdown not confirmed")),
                Err(error) => result = Err(error),
            }
        }
        if !unowned.is_empty() || !self.unowned()?.is_empty() {
            return Err(Error::Unresolved("an unowned JVM may still run"));
        }
        result
    }
}
#[tonic::async_trait]
impl Host for ProcessHost {
    fn configure(&self, endpoint: String) -> Result<()> {
        self.endpoint.set(endpoint).map_err(|_| Error::Invalid("control endpoint already set"))
    }
    async fn ensure(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Progress> {
        let process = match self.launch(id, release, app, profile) {
            Ok(Some(process)) => process,
            // Only the JVM's re-attachment or its free launch lock resolves a launch from before control restarted.
            Ok(None) if self.stopped(id) => return Ok(Progress::Failed("JVM exited while unowned".into())),
            Ok(None) => return Ok(Progress::Pending),
            Err(error @ Error::Unresolved(_)) => return Err(error),
            Err(error) => return Ok(Progress::Failed(error.to_string())),
        };
        if process.stopped.load(Ordering::Acquire) {
            return Ok(Progress::Failed("JVM exited".into()));
        }
        if process.stop.is_cancelled() {
            return Ok(Progress::Pending);
        }
        if let Some(connection) = process.connection() {
            return Ok(Progress::Ready(Box::new(connection)));
        }
        if process.launched.elapsed() >= Duration::from_secs(35) {
            process.stop.cancel();
            return Ok(Progress::Failed("app did not become ready within 35 seconds".into()));
        }
        Ok(Progress::Pending)
    }
    fn register(&self, token: &str, registration: Registration) -> Result<()> {
        let process = self.process(&registration.identity.host)?.ok_or(Error::Invalid("unknown process"))?;
        if token != format!("Bearer {}", process.token) {
            return Err(Error::Invalid("invalid process credential"));
        }
        if registration.identity != process.identity {
            return Err(Error::Invalid("process identity mismatch"));
        }
        if process.stop.is_cancelled() || process.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        self.validate_endpoint(&registration)?;
        let mut frozen = process.registration.lock().map_err(|_| Error::Unresolved("registration poisoned"))?;
        if frozen.as_ref().is_some_and(|previous| previous != &registration) {
            return Err(Error::Invalid("registration changed"));
        }
        *frozen = Some(registration);
        Ok(())
    }
    fn authenticate(&self, credential: &str) -> Option<String> {
        let processes = self.processes.lock().ok()?;
        let mut found = None;
        // Every running process is compared, so timing reveals no match position.
        for (id, process) in &processes.running {
            if chunk_service::same_secret(credential, &process.token)
                && !process.stop.is_cancelled()
                && !process.stopped.load(Ordering::Acquire)
            {
                found = Some(id.clone());
            }
        }
        found
    }
    fn unadopted(&self, credential: &str) -> Option<String> {
        let presented = digest(credential);
        let mut found = None;
        // Every awaiting launch is compared, so timing reveals no match position.
        for id in self.unowned().ok()? {
            if self
                .launch_record(&id)
                .is_some_and(|record| chunk_service::same_secret(&presented, &record.token_sha256))
            {
                found = Some(id);
            }
        }
        found
    }
    fn adopt(&self, token: &str, registration: Registration) -> Result<()> {
        self.validate_endpoint(&registration)?;
        let identity = registration.identity.clone();
        let id = identity.host.clone();
        let mut processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
        if processes.running.contains_key(&id)
            || processes.failed.contains(&id)
            || !self.path(&id, "launch")?.try_exists()?
            || self.path(&id, "exit")?.try_exists()?
        {
            return Err(Error::Invalid("process is not awaiting re-attachment"));
        }
        if !self.launch_record(&id).is_some_and(|record| record.authenticates(&identity, token)) {
            return Err(Error::Invalid("process credential does not match its launch record"));
        }
        let process = Process {
            identity,
            token: token.into(),
            registration: Mutex::new(Some(registration)),
            stop: CancellationToken::new(),
            stopped: AtomicBool::new(false),
            adopted: true,
            launched: Instant::now(),
        };
        processes.running.insert(id, Arc::new(process));
        Ok(())
    }
    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        self.process(id).ok()??.connection()
    }
    async fn release(&self, id: &str) -> Result<bool> {
        if self.stopped(id) {
            return Ok(true);
        }
        let process = {
            let processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
            if let Some(process) = processes.running.get(id) {
                process.clone()
            } else {
                if self.path(id, "exit")?.is_file() {
                    return Ok(true);
                }
                // An unowned launch's JVM still holds its marker's lock.
                if self.path(id, "launch")?.try_exists()? {
                    return Ok(false);
                }
                // Launch holds this same lock and checks the exit record before spawning.
                self.record_exit(id, b"never launched")?;
                return Ok(true);
            }
        };
        process.stop.cancel();
        // A re-attached JVM has no Child, so it is killed by its recorded PID after the same grace an owned one gets,
        // and only its launch marker can confirm it exited.
        if process.adopted
            && !self.exits(id, EXIT_GRACE).await
            && let Err(error) = pid::kill(&self.path(id, "pid")?)
            && !self.stopped(id)
        {
            tracing::warn!(%error, host = id, "cannot kill an adopted JVM; its release stays unresolved");
            return Ok(false);
        }
        Ok(self.exits(id, Duration::from_secs(12)).await)
    }
    fn unresolved(&self, id: &str) -> bool {
        // A marker that cannot be looked up may exist.
        self.process(id).ok().flatten().is_none()
            && self.path(id, "launch").is_ok_and(|path| path.try_exists().unwrap_or(true))
            && !self.stopped(id)
    }
    fn unowned(&self) -> Result<BTreeSet<String>> {
        Ok(self.recorded("launch")?.into_iter().filter(|id| self.unresolved(id)).collect())
    }
    fn stopped(&self, id: &str) -> bool {
        if self.path(id, "exit").is_ok_and(|p| p.is_file()) {
            return true;
        }
        // Holding the lock excludes a concurrent launch rewriting the marker.
        let Ok(processes) = self.processes.lock() else {
            return false;
        };
        if processes.failed.contains(id) {
            return true;
        }
        match processes.running.get(id) {
            Some(process) if process.stopped.load(Ordering::Acquire) => true,
            // Only a JVM without an owned Child needs its launch marker to confirm its exit.
            Some(process) if !process.adopted => false,
            _ => self.confirm_exit(id),
        }
    }

    fn prune(&self, retained: &BTreeSet<String>) -> Result<()> {
        let mut processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
        let mut stopped = processes.failed.clone();
        stopped.extend(
            processes
                .running
                .iter()
                .filter(|(_, process)| process.stopped.load(Ordering::Acquire))
                .map(|(id, _)| id.clone()),
        );
        // Exit records also recover cleanup interrupted after the durable host was removed.
        stopped.extend(self.recorded("exit")?);
        let mut result = Ok(());
        'hosts: for id in stopped.difference(retained) {
            for extension in ["launch", "launch.staged", "jvm.log", "pid", "exit"] {
                match std::fs::remove_file(self.path(id, extension)?) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => {
                        result = Err(error.into());
                        continue 'hosts;
                    }
                }
            }
            processes.running.remove(id);
            processes.failed.remove(id);
        }
        result
    }
}
async fn own_child(mut child: Child, process: &Process) -> io::Result<()> {
    tokio::select! {
        exit = child.wait() => { exit?; return Ok(()); }
        () = process.stop.cancelled() => {}
    }
    if let Ok(result) = tokio::time::timeout(EXIT_GRACE, child.wait()).await {
        result?;
    } else {
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}

/// What a launch marker records: the JVM's process identity, the SHA-256 digest of its credential, and the control
/// endpoint it registers at.
#[derive(serde::Serialize, serde::Deserialize)]
struct LaunchRecord {
    process_id: String,
    generation: u64,
    token_sha256: String,
    #[serde(default)]
    control_endpoint: String,
}

impl LaunchRecord {
    fn of(identity: &JvmIdentity, token: &str, control_endpoint: &str) -> Self {
        Self {
            process_id: identity.process_id.clone(),
            generation: identity.generation,
            token_sha256: digest(token),
            control_endpoint: control_endpoint.into(),
        }
    }

    fn authenticates(&self, identity: &JvmIdentity, token: &str) -> bool {
        self.process_id == identity.process_id
            && self.generation == identity.generation
            && self.token_sha256 == digest(token)
    }
}

/// Leaves `lock` open in the JVM as descriptor 3. No Java stream uses it, so app code cannot close it, and Java
/// closes it in the processes the JVM starts.
#[cfg(unix)]
fn inherit_lock(command: &mut Command, lock: File) -> io::Result<()> {
    use command_fds::{CommandFdExt, FdMapping};
    command.fd_mappings(vec![FdMapping { parent_fd: lock.into(), child_fd: 3 }]).map_err(io::Error::other)?;
    Ok(())
}

#[cfg(not(unix))]
fn inherit_lock(_command: &mut Command, _lock: File) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "JVM launches require Unix"))
}

/// Runs its arguments once control writes a line to its stdin, and nothing if control closes stdin first, as when it
/// crashes. The JVM then reads stdin at its end.
const GATE: &str = r#"read -r _ && exec "$@""#;

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

/// How long a JVM asked to stop may take to acknowledge it before control stops it without its help.
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(3);

/// How long a JVM asked to stop may take to exit before control kills it.
const EXIT_GRACE: Duration = Duration::from_secs(5);

mod classpath;
mod pid;

#[cfg(all(test, unix))]
mod tests;
