use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use chunk_proto::v1::{
    DeploymentRef, ProcessIdentity, ProcessInventory, gameplay_server::GameplayServer,
    process_control_client::ProcessControlClient, process_control_server::ProcessControlServer,
    supervisor_server::SupervisorServer,
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    sync::watch,
    task::JoinHandle,
    time::{interval, timeout},
};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;

use crate::{
    relay,
    service::{Service, Shared},
};

pub struct Launch {
    pub backend: Option<chunk_contract::BackendConnection>,
    /// The generated JVM distribution launcher or a Java executable with classpath arguments.
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub deployment: DeploymentRef,
    pub machine_profile: String,
    pub artifact_digest: String,
    pub log_path: PathBuf,
    pub startup_timeout: Duration,
    pub bootstrap_session: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Ready,
    Unreachable,
    Stopped,
    Failed,
}

#[derive(Debug, Clone)]
pub struct Status {
    pub phase: Phase,
    pub inventory: Option<ProcessInventory>,
    pub diagnostic: Option<String>,
}

/// Dropping the owner starts bounded shutdown. Prefer `stop().await` to await cleanup.
/// A failed lifecycle RPC leaves surviving player relays and their identities intact.
pub struct ManagedJvm {
    shared: Arc<Shared>,
    endpoint: String,
    task: Option<JoinHandle<io::Result<()>>>,
    failure: Option<Arc<io::Error>>,
}

/// A launch can fail either before spawning/after confirmed cleanup, or with cleanup unresolved.
#[derive(Debug)]
pub enum LaunchError {
    Stopped(io::Error),
    Unresolved(io::Error),
}

impl std::fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped(error) | Self::Unresolved(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Stopped(error) | Self::Unresolved(error) => Some(error),
        }
    }
}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self::Stopped(error)
    }
}

impl From<LaunchError> for io::Error {
    fn from(error: LaunchError) -> Self {
        match error {
            LaunchError::Stopped(error) | LaunchError::Unresolved(error) => error,
        }
    }
}

impl ManagedJvm {
    /// Launches one scoped JVM and waits for authenticated registration plus advancing ticks.
    /// # Errors
    /// Returns `Stopped` before spawn or after confirmed cleanup; `Unresolved` if cleanup fails.
    pub async fn launch(launch: Launch) -> Result<Self, LaunchError> {
        if launch.startup_timeout.is_zero()
            || launch.startup_timeout > Duration::from_secs(120)
            || launch.deployment.environment.is_empty()
            || launch.deployment.deployment.is_empty()
            || launch.machine_profile.is_empty()
            || launch.artifact_digest.is_empty()
        {
            return Err(io::Error::other("invalid JVM launch settings").into());
        }
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let ingress = TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let identity = ProcessIdentity {
            deployment: Some(launch.deployment.clone()),
            runtime_id: uuid::Uuid::new_v4().to_string(),
            process_id: uuid::Uuid::new_v4().to_string(),
            generation: 1,
            machine_profile: launch.machine_profile.clone(),
            artifact_digest: launch.artifact_digest.clone(),
        };
        let (status, mut receiver) = watch::channel(Status {
            phase: Phase::Starting,
            inventory: None,
            diagnostic: None,
        });
        let shared = Arc::new(Shared {
            identity,
            child_credential: credential(),
            credential: credential(),
            ingress: ingress.local_addr()?,
            registration: Mutex::new(None),
            bindings: Mutex::new(BTreeMap::new()),
            shutdown: CancellationToken::new(),
            status,
        });
        let child = spawn_jvm(&launch, &shared, &endpoint)?;
        tracing::info!(runtime_id = %shared.identity.runtime_id, pid = child.id(), "launched gameplay JVM");
        let task = tokio::spawn(monitor(shared.clone(), child, listener, ingress));
        let process = Self {
            shared,
            endpoint,
            task: Some(task),
            failure: None,
        };
        let ready = timeout(launch.startup_timeout, async {
            receiver
                .wait_for(|status| matches!(status.phase, Phase::Ready | Phase::Failed | Phase::Stopped))
                .await
                .is_ok_and(|status| status.phase == Phase::Ready)
        })
        .await
        .unwrap_or(false);
        if ready {
            Ok(process)
        } else {
            let diagnostic = process
                .status()
                .diagnostic
                .unwrap_or_else(|| "JVM registration/readiness deadline".into());
            process.stop().await.map_err(LaunchError::Unresolved)?;
            Err(io::Error::other(format!("{diagnostic}; diagnostics: {}", launch.log_path.display())).into())
        }
    }

    #[must_use]
    pub fn identity(&self) -> &ProcessIdentity {
        &self.shared.identity
    }
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    /// Runtime-facing credential; the launched JVM's credential is never exposed here.
    #[must_use]
    pub fn credential(&self) -> &str {
        &self.shared.credential
    }
    #[must_use]
    pub fn status(&self) -> Status {
        self.shared.status.borrow().clone()
    }
    #[must_use]
    pub fn watch(&self) -> watch::Receiver<Status> {
        self.shared.status.subscribe()
    }

    /// # Errors
    /// Reports unavailable lifecycle channels without replacing player streams.
    pub async fn inventory(&self) -> Result<ProcessInventory, tonic::Status> {
        self.shared.inventory().await
    }

    /// # Errors
    /// Reports monitor failure or an unconfirmed JVM exit.
    pub async fn stop(mut self) -> io::Result<()> {
        self.shared.shutdown.cancel();
        self.wait().await
    }

    /// Awaits monitor completion without requesting shutdown.
    /// # Errors
    /// Reports monitor failure or an unconfirmed JVM exit.
    pub async fn wait(&mut self) -> io::Result<()> {
        if let Some(task) = self.task.as_mut() {
            let result = task.await.map_err(io::Error::other).and_then(std::convert::identity);
            self.task.take();
            self.failure = result.err().map(Arc::new);
        }
        match &self.failure {
            Some(error) => Err(io::Error::new(error.kind(), error.clone())),
            None => Ok(()),
        }
    }
}

impl Drop for ManagedJvm {
    fn drop(&mut self) {
        self.shared.shutdown.cancel();
    }
}

fn credential() -> String {
    format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())
}

pub(crate) async fn monitor(
    shared: Arc<Shared>,
    mut child: Child,
    listener: TcpListener,
    ingress: TcpListener,
) -> io::Result<()> {
    let service = Service(shared.clone());
    let stop_server = shared.shutdown.clone();
    let mut server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(SupervisorServer::new(service.clone()).max_decoding_message_size(8 * 1024 * 1024))
            .add_service(GameplayServer::new(service.clone()).max_encoding_message_size(8 * 1024 * 1024))
            .add_service(ProcessControlServer::new(service).max_encoding_message_size(8 * 1024 * 1024))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop_server.cancelled_owned())
            .await
    });
    let mut relay = tokio::spawn(relay::accept(ingress, shared.clone()));
    let mut poll = interval(Duration::from_secs(1));
    let mut tick_count = 0;
    let mut advanced = Instant::now();
    let mut server_done = false;
    let mut relay_done = false;
    let failure = loop {
        tokio::select! {
            biased;
            () = shared.shutdown.cancelled() => break None,
            result = child.wait() => break Some(format!("JVM exited: {result:?}")),
            result = &mut server => { server_done = true; break Some(format!("runtime gRPC server stopped: {result:?}")); },
            result = &mut relay => { relay_done = true; break Some(format!("runtime TCP acceptor stopped: {result:?}")); },
            _ = poll.tick() => {
                if shared.registered().is_err() { continue; }
                match shared.inventory().await {
                    Ok(inventory) => {
                        if inventory.tick_count > tick_count {
                            tick_count = inventory.tick_count;
                            advanced = Instant::now();
                        } else if advanced.elapsed() > Duration::from_secs(5) {
                            break Some("JVM ticks stopped advancing".into());
                        }
                        if tick_count > 0 { shared.status.send_replace(Status { phase: Phase::Ready, inventory: Some(inventory), diagnostic: None }); }
                    }
                    Err(error) => {
                        let inventory = shared.status.borrow().inventory.clone();
                        shared.status.send_replace(Status { phase: Phase::Unreachable, inventory, diagnostic: Some(format!("JVM lifecycle channel: {}", error.code())) });
                        // A later successful probe must observe subsequent tick progress before
                        // deciding the simulation stalled during this control-channel outage.
                        advanced = Instant::now();
                    }
                }
            }
        }
    };
    shared.shutdown.cancel();
    let stopped = stop_child(&shared, &mut child).await;
    let phase = if failure.is_some() || stopped.is_err() {
        Phase::Failed
    } else {
        Phase::Stopped
    };
    shared.status.send_replace(Status {
        phase,
        inventory: None,
        diagnostic: failure,
    });
    if !server_done && timeout(Duration::from_secs(5), &mut server).await.is_err() {
        server.abort();
        let _ = server.await;
    }
    if !relay_done && timeout(Duration::from_secs(5), &mut relay).await.is_err() {
        relay.abort();
        let _ = relay.await;
    }
    stopped
}

async fn stop_child(shared: &Shared, child: &mut Child) -> io::Result<()> {
    if let Ok(registered) = shared.registered() {
        let _ = ProcessControlClient::new(registered.channel)
            .stop_process(shared.request(shared.identity.clone()))
            .await;
    }
    if let Ok(result) = timeout(Duration::from_secs(5), child.wait()).await {
        result?;
    } else {
        child.start_kill()?;
        child.wait().await?;
    }

    Ok(())
}

fn spawn_jvm(launch: &Launch, shared: &Shared, endpoint: &str) -> io::Result<Child> {
    let mut log = OpenOptions::new();
    log.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log.mode(0o600);
    }
    let log = log.open(&launch.log_path)?;
    let deployment = &launch.deployment;
    let mut command = Command::new(&launch.program);
    command
        .env_remove("CHUNK_BACKEND_ENDPOINT")
        .env_remove("CHUNK_BACKEND_TOKEN");
    if let Some(backend) = &launch.backend {
        if backend.environment != launch.deployment.environment || backend.deployment != launch.deployment.deployment {
            return Err(io::Error::other("backend deployment mismatch"));
        }
        command
            .env("CHUNK_BACKEND_ENDPOINT", &backend.endpoint)
            .env("CHUNK_BACKEND_TOKEN", &backend.token);
    }
    command
        .args(&launch.arguments)
        .env("CHUNK_SUPERVISOR", endpoint)
        .env("CHUNK_PROCESS_TOKEN", &shared.child_credential)
        .env("CHUNK_ENVIRONMENT", &deployment.environment)
        .env("CHUNK_DEPLOYMENT", &deployment.deployment)
        .env("CHUNK_RUNTIME_ID", &shared.identity.runtime_id)
        .env("CHUNK_PROCESS_ID", &shared.identity.process_id)
        .env("CHUNK_PROCESS_GENERATION", shared.identity.generation.to_string())
        .env("CHUNK_MACHINE_PROFILE", &shared.identity.machine_profile)
        .env("CHUNK_ARTIFACT_DIGEST", &shared.identity.artifact_digest)
        .env(
            "CHUNK_BOOTSTRAP_SESSION",
            if launch.bootstrap_session { "bridge" } else { "" },
        )
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn repeated_wait_and_stop_preserve_the_monitor_outcome() {
        let ingress = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (shared, _, _, stop, server) = crate::tests::fixture(&ingress).await;
        for outcome in 0..3 {
            let task = tokio::spawn(async move {
                match outcome {
                    0 => Ok(()),
                    1 => Err(io::Error::other("unconfirmed exit")),
                    _ => panic!("monitor failure"),
                }
            });
            let mut process = ManagedJvm {
                shared: shared.clone(),
                endpoint: "http://127.0.0.1:1".into(),
                task: Some(task),
                failure: None,
            };
            assert_eq!(process.wait().await.is_ok(), outcome == 0);
            assert_eq!(process.wait().await.is_ok(), outcome == 0);
            assert_eq!(process.stop().await.is_ok(), outcome == 0);
        }
        stop.cancel();
        server.await.unwrap();
    }
}
