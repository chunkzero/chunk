use crate::{
    Distribution, Error, Host, ProcessHostConfig, Progress, Release, Result, RuntimeConnection,
    client::{auth, channel},
};
use chunk_proto::v1::{ProcessIdentity, ProcessRegistration, node_control_client::NodeControlClient};
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
    /// Each launchable release's distribution, by deployment version.
    distributions: Mutex<BTreeMap<String, Distribution>>,
    processes: Mutex<Processes>,
}
#[derive(Default)]
struct Processes {
    running: BTreeMap<String, Arc<Process>>,
    failed: BTreeSet<String>,
}
struct Process {
    identity: ProcessIdentity,
    token: String,
    registration: Mutex<Option<ProcessRegistration>>,
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
            endpoint: registration.control_endpoint,
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
        Self { config, endpoint: OnceLock::new(), distributions: Mutex::default(), processes: Mutex::default() }
    }
    /// Launches JVMs of `deployment`'s release from `distribution`.
    /// # Errors
    /// Reports a poisoned host.
    pub fn add_release(&self, deployment: &str, distribution: Distribution) -> Result<()> {
        let mut distributions = self.distributions.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
        distributions.insert(deployment.into(), distribution);
        Ok(())
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
            if process.identity.deployment.as_ref() != Some(&release.deployment)
                || process.identity.app_id != app
                || process.identity.machine_profile != profile
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
        match self.start(id, release, app, profile) {
            Ok(process) => {
                processes.running.insert(id.into(), process.clone());
                Ok(Some(process))
            }
            Err(error) => {
                // No child was spawned, and the launch lock excludes another attempt for this ID.
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
        let distribution = self
            .distributions
            .lock()
            .map_err(|_| Error::Unresolved("host poisoned"))?
            .get(&deployment.deployment)
            .cloned()
            .ok_or(Error::Invalid("unknown release distribution"))?;
        let artifact = release.apps.get(app).ok_or(Error::Invalid("unknown app"))?;
        // Placement already bound the profile to the app's session or one of its declared destinations.
        let size = release.profiles.get(profile).ok_or(Error::Invalid("unknown profile"))?;
        let backend = &self.config.backend;
        if backend.environment != deployment.environment {
            return Err(Error::Invalid("gameplay backend scope mismatch"));
        }
        let root = distribution.directory.canonicalize()?;
        let jar = distribution.directory.join(&artifact.jar).canonicalize()?;
        let bytes = std::fs::read(&jar)?;
        if !jar.starts_with(&root) || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(Error::Invalid("app artifact digest mismatch"));
        }
        classpath::verify(&root, &jar, &bytes)?;
        std::fs::create_dir_all(&self.config.directory)?;
        let log_path = self.path(id, "jvm.log")?;
        let exit = self.path(id, "exit")?;
        let process = Arc::new(Process {
            identity: ProcessIdentity {
                deployment: Some(deployment.clone()),
                runtime_id: id.into(),
                process_id: uuid::Uuid::new_v4().to_string(),
                generation: 1,
                machine_profile: profile.into(),
                artifact_digest: artifact.sha256.clone(),
                app_id: app.into(),
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
        let marker = self.record_launch(id, &LaunchRecord::of(&process.identity, &process.token))?;
        let child = (|| {
            let log = chunk_service::private_file(&log_path)?;
            let mut command = Command::new(&distribution.java);
            command
                .arg(format!("-Xmx{}m", size.memory_mib))
                .arg("-jar")
                .arg(&jar)
                .env("CHUNK_PROCESS_TOKEN", &process.token)
                .env("CHUNK_ENVIRONMENT", &deployment.environment)
                .env("CHUNK_DEPLOYMENT", &deployment.deployment)
                .env("CHUNK_CONTROL_ENDPOINT", endpoint)
                .env("CHUNK_INSTANCE_ID", id)
                .env("CHUNK_PROCESS_ID", &process.identity.process_id)
                .env("CHUNK_PROCESS_GENERATION", "1")
                .env("CHUNK_MACHINE_PROFILE", profile)
                .env("CHUNK_ARTIFACT_DIGEST", &artifact.sha256)
                .env("CHUNK_APP_ID", app)
                .env("CHUNK_BACKEND_ENDPOINT", &backend.endpoint)
                .env("CHUNK_BACKEND_TOKEN", &backend.token)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .kill_on_drop(true);
            inherit_lock(&mut command, marker)?;
            command.spawn()
        })();
        let child = child?;
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
    /// Stops all owned JVMs, including launches awaiting readiness.
    /// # Errors
    /// Reports unconfirmed process exits.
    pub async fn shutdown(&self) -> Result<()> {
        let ids: Vec<_> =
            self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?.running.keys().cloned().collect();
        let mut result = Ok(());
        for id in ids {
            match self.release(&id).await {
                Ok(true) => {}
                Ok(false) => result = Err(Error::Unresolved("JVM shutdown not confirmed")),
                Err(error) => result = Err(error),
            }
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
    fn register(&self, token: &str, registration: ProcessRegistration) -> Result<ProcessIdentity> {
        let identity = registration.identity.as_ref().ok_or(Error::Invalid("missing process identity"))?;
        let process = self.process(&identity.runtime_id)?.ok_or(Error::Invalid("unknown process"))?;
        if token != format!("Bearer {}", process.token) {
            return Err(Error::Invalid("invalid process credential"));
        }
        if *identity != process.identity {
            return Err(Error::Invalid("process identity mismatch"));
        }
        if process.stop.is_cancelled() || process.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        validate_endpoints(&registration)?;
        let mut frozen = process.registration.lock().map_err(|_| Error::Unresolved("registration poisoned"))?;
        if frozen.as_ref().is_some_and(|previous| previous != &registration) {
            return Err(Error::Invalid("registration changed"));
        }
        *frozen = Some(registration);
        Ok(process.identity.clone())
    }
    fn adopt(&self, token: &str, registration: ProcessRegistration) -> Result<()> {
        validate_endpoints(&registration)?;
        let identity = registration.identity.clone().ok_or(Error::Invalid("missing process identity"))?;
        let id = identity.runtime_id.clone();
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
        // A re-attached JVM has no Child to stop it, so only its launch marker can confirm it exited.
        if process.adopted {
            stop_gracefully(&process).await;
        }
        let deadline = Instant::now() + Duration::from_secs(12);
        while !self.stopped(id) {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            sleep(Duration::from_millis(25)).await;
        }
        Ok(true)
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
            for extension in ["launch", "launch.staged", "jvm.log", "exit"] {
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
    stop_gracefully(process).await;
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        result?;
    } else {
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}

/// What a launch marker records: the JVM's process identity and the SHA-256 digest of its credential.
#[derive(serde::Serialize, serde::Deserialize)]
struct LaunchRecord {
    process_id: String,
    generation: u64,
    token_sha256: String,
}

impl LaunchRecord {
    fn of(identity: &ProcessIdentity, token: &str) -> Self {
        Self { process_id: identity.process_id.clone(), generation: identity.generation, token_sha256: digest(token) }
    }

    fn authenticates(&self, identity: &ProcessIdentity, token: &str) -> bool {
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

fn digest(token: &str) -> String {
    format!("{:x}", Sha256::digest(token.as_bytes()))
}

async fn stop_gracefully(process: &Process) {
    if let Some(connection) = process.connection() {
        let graceful = async {
            NodeControlClient::new(channel(&connection).await?)
                .stop_process(auth(&connection, connection.identity.clone(), 2)?)
                .await?;
            Ok::<_, Error>(())
        };
        let _ = tokio::time::timeout(Duration::from_secs(3), graceful).await;
    }
}

fn validate_endpoints(registration: &ProcessRegistration) -> Result<()> {
    for endpoint in [&registration.control_endpoint, &registration.player_endpoint] {
        let address: std::net::SocketAddr = endpoint
            .strip_prefix("http://")
            .unwrap_or(endpoint)
            .parse()
            .map_err(|_| Error::Invalid("process endpoint"))?;
        if !address.ip().is_loopback() || address.port() == 0 {
            return Err(Error::Invalid("process requires loopback"));
        }
    }
    if !registration.control_endpoint.starts_with("http://") {
        return Err(Error::Invalid("control endpoint scheme"));
    }
    Ok(())
}

mod classpath;

#[cfg(all(test, unix))]
mod tests;
