use crate::{
    Error, Host, ProcessHostConfig, Result, RuntimeConnection,
    client::{auth, channel},
};
use chunk_proto::v1::{ProcessIdentity, ProcessRegistration, node_control_client::NodeControlClient};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
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
    processes: Mutex<BTreeMap<String, Arc<Process>>>,
}
struct Process {
    identity: ProcessIdentity,
    token: String,
    registration: Mutex<Option<ProcessRegistration>>,
    stop: CancellationToken,
    stopped: AtomicBool,
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
            for process in processes.values() {
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
        Ok(self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?.get(id).cloned())
    }
    fn launch(&self, id: &str, app: &str, profile: &str) -> Result<Arc<Process>> {
        let mut processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
        if let Some(process) = processes.get(id) {
            if process.identity.app_id != app || process.identity.machine_profile != profile {
                return Err(Error::Invalid("host binding changed"));
            }
            return Ok(process.clone());
        }
        let endpoint = self.endpoint.get().ok_or(Error::Unresolved("control not listening"))?;
        let artifact = self.config.apps.get(app).ok_or(Error::Invalid("unknown app"))?;
        // Placement already bound the profile to the app's session or one of its declared destinations.
        let size = self.config.profiles.get(profile).ok_or(Error::Invalid("unknown profile"))?;
        let backend = &self.config.backend;
        if backend.environment != self.config.deployment.environment
            || backend.deployment != self.config.deployment.deployment
        {
            return Err(Error::Invalid("gameplay backend scope mismatch"));
        }
        let distribution = self.config.distribution.canonicalize()?;
        let jar = self.config.distribution.join(&artifact.jar).canonicalize()?;
        let bytes = std::fs::read(&jar)?;
        if !jar.starts_with(&distribution) || format!("{:x}", Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(Error::Invalid("app artifact digest mismatch"));
        }
        classpath::verify(&distribution, &jar, &bytes)?;
        std::fs::create_dir_all(&self.config.directory)?;
        if self.path(id, "exit")?.exists() {
            return Err(Error::Stopped);
        }
        // A launch marker without an owned Child leaves termination unconfirmed instead of stopped.
        let _marker = chunk_service::private_file(&self.path(id, "launch")?)?;
        let log_path = self.path(id, "jvm.log")?;
        let exit = self.path(id, "exit")?;
        let process = Arc::new(Process {
            identity: ProcessIdentity {
                deployment: Some(self.config.deployment.clone()),
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
        });
        let child = (|| {
            let log = chunk_service::private_file(&log_path)?;
            Command::new(&self.config.java)
                .arg(format!("-Xmx{}m", size.memory_mib))
                .arg("-jar")
                .arg(&jar)
                .env("CHUNK_PROCESS_TOKEN", &process.token)
                .env("CHUNK_ENVIRONMENT", &self.config.deployment.environment)
                .env("CHUNK_DEPLOYMENT", &self.config.deployment.deployment)
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
                .kill_on_drop(true)
                .spawn()
        })();
        let child = match child {
            Ok(child) => child,
            Err(error) => {
                std::fs::write(exit, b"spawn failed")?;
                return Err(error.into());
            }
        };
        processes.insert(id.into(), process.clone());
        let owned = process.clone();
        tokio::spawn(async move {
            match own_child(child, &owned).await {
                Ok(()) => {
                    owned.stopped.store(true, Ordering::Release);
                    if let Err(error) = std::fs::write(exit, b"stopped") {
                        tracing::error!(%error, "cannot persist JVM exit");
                    }
                }
                Err(error) => tracing::error!(%error, "JVM exit is unconfirmed"),
            }
        });
        Ok(process)
    }
    /// Stops all owned JVMs, including launches awaiting readiness.
    /// # Errors
    /// Reports unconfirmed process exits.
    pub async fn shutdown(&self) -> Result<()> {
        let ids: Vec<_> =
            self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?.keys().cloned().collect();
        let mut result = Ok(());
        for id in ids {
            if let Err(error) = self.terminate(&id).await {
                result = Err(error);
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
    async fn ensure(&self, id: &str, app: &str, profile: &str) -> Result<RuntimeConnection> {
        let process = self.launch(id, app, profile)?;
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if process.stopped.load(Ordering::Acquire) {
                return Err(Error::Stopped);
            }
            if process.stop.is_cancelled() {
                return Err(Error::Unresolved("process stopping"));
            }
            if let Some(connection) = process.connection() {
                return Ok(connection);
            }
            if Instant::now() >= deadline {
                process.stop.cancel();
                return Err(Error::Unresolved("app did not become ready within 35 seconds"));
            }
            sleep(Duration::from_millis(25)).await;
        }
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
        let mut frozen = process.registration.lock().map_err(|_| Error::Unresolved("registration poisoned"))?;
        if frozen.as_ref().is_some_and(|previous| previous != &registration) {
            return Err(Error::Invalid("registration changed"));
        }
        *frozen = Some(registration);
        Ok(process.identity.clone())
    }
    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        self.process(id).ok()??.connection()
    }
    async fn terminate(&self, id: &str) -> Result<()> {
        if self.stopped(id) {
            return Ok(());
        }
        let process = {
            let processes = self.processes.lock().map_err(|_| Error::Unresolved("host poisoned"))?;
            if let Some(process) = processes.get(id) {
                process.clone()
            } else {
                if self.path(id, "exit")?.is_file() {
                    return Ok(());
                }
                if self.path(id, "launch")?.try_exists()? {
                    return Err(Error::Unresolved("no owned process; exit unconfirmed"));
                }
                // Launch holds this same lock and checks the exit record before spawning.
                std::fs::create_dir_all(&self.config.directory)?;
                let mut exit = chunk_service::private_file(&self.path(id, "exit")?)?;
                exit.write_all(b"never launched")?;
                exit.sync_all()?;
                return Ok(());
            }
        };
        process.stop.cancel();
        let deadline = Instant::now() + Duration::from_secs(12);
        while !process.stopped.load(Ordering::Acquire) {
            if Instant::now() >= deadline {
                return Err(Error::Unresolved("JVM shutdown not confirmed"));
            }
            sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }
    fn unresolved(&self, id: &str) -> bool {
        self.process(id).ok().flatten().is_none()
            && self.path(id, "launch").is_ok_and(|p| p.exists())
            && !self.stopped(id)
    }
    fn stopped(&self, id: &str) -> bool {
        self.process(id).ok().flatten().is_some_and(|p| p.stopped.load(Ordering::Acquire))
            || self.path(id, "exit").is_ok_and(|p| p.is_file())
    }
}
async fn own_child(mut child: Child, process: &Process) -> io::Result<()> {
    tokio::select! {
        exit = child.wait() => { exit?; return Ok(()); }
        () = process.stop.cancelled() => {}
    }
    if let Some(connection) = process.connection() {
        let graceful = async {
            NodeControlClient::new(channel(&connection).await?)
                .stop_process(auth(&connection, connection.identity.clone(), 2)?)
                .await?;
            Ok::<_, Error>(())
        };
        let _ = tokio::time::timeout(Duration::from_secs(3), graceful).await;
    }
    if let Ok(result) = tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        result?;
    } else {
        child.kill().await?;
        child.wait().await?;
    }
    Ok(())
}

mod classpath;

#[cfg(all(test, unix))]
mod tests;
