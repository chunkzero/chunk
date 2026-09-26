use super::{Reporter, Settings, Staged, report::Destination};
use chunk_build::Release;
use chunk_contract::{BackendConnection, ControlConnection};
use chunk_proto::v1::{
    MovePlayerRequest, NodePhase, NodeStatus, NodesRequest, PlayerStatus, PlayersRequest, SessionDemand,
};
use std::{io, path::PathBuf, sync::Arc, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

type Task = JoinHandle<io::Result<()>>;

const STARTUP: Duration = Duration::from_secs(30);

struct Service {
    stop: CancellationToken,
    task: Task,
}
impl Service {
    async fn ready<T>(slot: &mut Option<Self>, started: oneshot::Receiver<T>, name: &str) -> io::Result<T> {
        match tokio::time::timeout(STARTUP, started).await {
            Ok(Ok(connection)) => return Ok(connection),
            Ok(Err(_)) => {}
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{name} did not become ready within {}s", STARTUP.as_secs()),
                ));
            }
        }
        let result = (&mut slot.as_mut().expect("service started").task).await;
        slot.take();
        match result {
            Ok(Err(error)) => Err(error),
            Err(error) => Err(io::Error::other(error)),
            Ok(Ok(())) => Err(io::Error::other(format!("{name} stopped before readiness"))),
        }
    }

    async fn stop(self) -> io::Result<()> {
        self.stop.cancel();
        self.task.await.map_err(io::Error::other)?
    }
}

/// The backend, control and proxy that outlive every release.
#[derive(Default)]
pub(super) struct Shared {
    backend: Option<Service>,
    handle: Option<chunk_backend::Backend>,
    connection: Option<BackendConnection>,
    control: Option<Service>,
    host: Option<Arc<chunk_control::ProcessHost>>,
    authority: Option<chunk_control::server::Ready>,
    edge: Option<Service>,
    retarget: Option<chunk_edge::Retarget>,
}

impl Shared {
    async fn start_backend(&mut self, settings: &Settings, staged: &Staged, reporter: &Reporter) -> io::Result<()> {
        let bundle = settings.state.join("deployment.json");
        std::fs::write(&bundle, serde_json::to_vec(&staged.bundle).map_err(io::Error::other)?)?;
        let token = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let config = chunk_backend::server::Config {
            bundle,
            environment: staged.control.deployment.environment.clone(),
            state: settings.state.join("backend"),
            connection: settings.state.join("backend.json"),
            bind: settings.backend_bind,
        };
        self.backend =
            Some(Service { task: tokio::spawn(chunk_backend::server::run(config, ready, token.clone())), stop: token });
        let ready = Service::ready(&mut self.backend, started, "backend").await?;
        reporter.done("Backend", &ready.connection.endpoint);
        self.handle = Some(ready.backend);
        self.connection = Some(ready.connection);
        Ok(())
    }

    /// Starts the environment's control, which drops the rows of an earlier session, whose JVMs are gone.
    async fn start_control(&mut self, settings: &Settings, environment: String) -> io::Result<()> {
        let state = settings.state.join("control");
        let backend = self.connection.clone().ok_or_else(|| io::Error::other("backend is not running"))?;
        let host = Arc::new(chunk_control::ProcessHost::new(chunk_control::ProcessHostConfig {
            directory: state.join("nodes"),
            backend,
        }));
        self.host = Some(host.clone());
        let token = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let config = chunk_control::server::Config {
            connection: state.join("connection.json"),
            state,
            system: self.system()?,
            bind: settings.control_bind,
            control: chunk_control::Config { environment },
            host,
            fresh: true,
        };
        self.control =
            Some(Service { task: tokio::spawn(chunk_control::server::run(config, ready, token.clone())), stop: token });
        self.authority = Some(Service::ready(&mut self.control, started, "control").await?);
        Ok(())
    }

    async fn start_proxy(&mut self, settings: &Settings, target: chunk_edge::PlatformTarget) -> io::Result<()> {
        let proxy = chunk_edge::Proxy::bind(
            settings.bind,
            chunk_edge::ProxyConfig { platform: Some(target), ..Default::default() },
        )
        .await?;
        self.retarget = proxy.retarget();
        let token = CancellationToken::new();
        let shutdown = token.clone();
        self.edge = Some(Service {
            task: tokio::spawn(proxy.run(async move {
                shutdown.cancelled().await;
                Ok(())
            })),
            stop: token,
        });
        Ok(())
    }

    fn backend_connection(&self, deployment: &str) -> io::Result<BackendConnection> {
        let connection = self.connection.as_ref().ok_or_else(|| io::Error::other("backend is not running"))?;
        Ok(BackendConnection { deployment: deployment.into(), ..connection.clone() })
    }

    pub fn backend(&self) -> Option<chunk_backend::Backend> {
        self.handle.clone()
    }

    /// The environment store's system lane, which control writes through.
    fn system(&self) -> io::Result<chunk_backend::System> {
        self.handle
            .as_ref()
            .map(chunk_backend::Backend::system)
            .ok_or_else(|| io::Error::other("backend is not running"))
    }

    fn authority(&self) -> io::Result<&chunk_control::server::Ready> {
        self.authority.as_ref().ok_or_else(|| io::Error::other("control is not running"))
    }

    /// Control's endpoint, for the proxy and for `chunk players` and `chunk nodes`.
    pub fn control_connection(&self) -> io::Result<&ControlConnection> {
        Ok(&self.authority()?.connection)
    }

    pub fn control(&self) -> io::Result<Arc<chunk_control::Control>> {
        Ok(self.authority()?.control.clone())
    }

    /// Makes `bundle` resident beside earlier versions, retrying while the backend is busy.
    pub async fn deploy(&self, bundle: chunk_contract::Deployment) -> io::Result<()> {
        let backend = self.handle.as_ref().ok_or_else(|| io::Error::other("backend is not running"))?;
        for _ in 0..50 {
            match backend.deploy(bundle.clone()).await {
                Err(chunk_backend::Error::Busy) => tokio::time::sleep(Duration::from_millis(200)).await,
                result => return result.map_err(io::Error::other),
            }
        }
        Err(io::Error::other("backend stayed busy for 10s; deployment not activated"))
    }

    /// Makes `version` control's current release, whose JVMs launch from its release directory. Earlier releases keep
    /// their sessions.
    pub fn activate(&self, version: &Version) -> io::Result<()> {
        let host = self.host.as_ref().ok_or_else(|| io::Error::other("control is not running"))?;
        let distribution =
            chunk_control::Distribution { directory: version.release.directory.clone(), java: version.java.clone() };
        host.add_release(&version.deployment, distribution).map_err(io::Error::other)?;
        self.control()?.activate_release(version.control.clone()).map_err(io::Error::other)
    }

    /// Sends later player connections to `version`'s backend.
    pub fn route(&self, version: &Version) -> io::Result<()> {
        let retarget = self.retarget.as_ref().ok_or_else(|| io::Error::other("proxy is not running"))?;
        retarget.replace(self.target(version)?)
    }

    fn target(&self, version: &Version) -> io::Result<chunk_edge::PlatformTarget> {
        let control = self.control_connection()?.clone();
        Ok(chunk_edge::PlatformTarget { backend: self.backend_connection(&version.deployment)?, control })
    }

    /// Whether the backend or proxy stopped.
    pub fn failed(&self) -> bool {
        [&self.backend, &self.edge].into_iter().flatten().any(|service| service.task.is_finished())
    }

    pub fn control_failed(&self) -> bool {
        self.control.as_ref().is_some_and(|service| service.task.is_finished())
    }

    /// Closes player connections first so releases can stop without new arrivals.
    pub async fn stop_proxy(&mut self) -> io::Result<()> {
        self.retarget = None;
        match self.edge.take() {
            Some(edge) => edge.stop().await.inspect_err(|error| tracing::error!(%error, "proxy shutdown failed")),
            None => Ok(()),
        }
    }

    /// Stops every JVM, then control, retrying until each JVM has confirmed its exit, however long that takes. The
    /// host and backend stay until then.
    pub async fn stop_control(&mut self, reporter: &Reporter) -> io::Result<()> {
        let mut waiting = false;
        if let Ok(control) = self.control() {
            // While control serves, a JVM that outlived an earlier control can still re-attach and be stopped.
            while let Err(error) = control.shutdown().await
                && !self.control_failed()
            {
                wait_for_jvms(reporter, &mut waiting, &error).await;
            }
        }
        self.authority = None;
        let mut result = Ok(());
        if let Some(control) = self.control.take()
            && let Err(error) = control.stop().await
        {
            tracing::error!(%error, "control shutdown failed");
            result = Err(error);
        }
        if let Some(host) = &self.host {
            while let Err(error) = host.shutdown().await {
                wait_for_jvms(reporter, &mut waiting, &error).await;
            }
        }
        self.host = None;
        result
    }

    pub async fn stop(mut self, reporter: &Reporter) -> io::Result<()> {
        let mut result = self.stop_proxy().await;
        if let Err(error) = self.stop_control(reporter).await {
            result = Err(error);
        }
        // The service joins the backend engine only once this last outside handle is gone.
        if let Some(handle) = self.handle.take() {
            tokio::task::spawn_blocking(move || drop(handle)).await.map_err(io::Error::other)?;
        }
        if let Some(backend) = self.backend.take()
            && let Err(error) = backend.stop().await
        {
            result = Err(error);
        }
        result
    }
}

/// Waits before the next attempt to stop the JVMs, telling the user once that it waits for them.
async fn wait_for_jvms(reporter: &Reporter, waiting: &mut bool, error: &chunk_control::Error) {
    if !std::mem::replace(waiting, true) {
        tracing::warn!(%error, "JVM exit unconfirmed; retrying until every JVM stops");
        reporter.running("Stop", "waiting for JVMs to stop");
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
}

/// One deployment version: its release, its backend version, and its release in control.
pub(super) struct Version {
    pub release: Release,
    pub deployment: String,
    pub destinations: Vec<Destination>,
    control: chunk_control::Release,
    java: PathBuf,
}

impl Version {
    pub fn new(staged: Staged) -> Self {
        Self {
            destinations: destinations(staged.control.contracts.destinations.as_ref()),
            release: staged.release,
            deployment: staged.bundle.id,
            control: staged.control,
            java: staged.java,
        }
    }
}

/// Starts the backend, control with its first release, and the proxy; on error, everything started is stopped.
pub(super) async fn start(settings: &Settings, staged: Staged, reporter: &Reporter) -> io::Result<(Shared, Version)> {
    let mut shared = Shared::default();
    reporter.running("Backend", settings.backend_bind);
    if let Err(error) = shared.start_backend(settings, &staged, reporter).await {
        return Err(abandon(error, shared, reporter).await);
    }
    reporter.running("Control", settings.control_bind);
    let environment = staged.control.deployment.environment.clone();
    let version = Version::new(staged);
    let started = match shared.start_control(settings, environment).await {
        Ok(()) => shared.activate(&version),
        Err(error) => Err(error),
    };
    if let Err(error) = started {
        return Err(abandon(error, shared, reporter).await);
    }
    if let Ok(connection) = shared.control_connection() {
        reporter.done("Control", &connection.endpoint);
    }
    reporter.running("Proxy", settings.bind);
    let proxy = match shared.target(&version) {
        Ok(target) => shared.start_proxy(settings, target).await,
        Err(error) => Err(error),
    };
    if let Err(error) = proxy {
        return Err(abandon(error, shared, reporter).await);
    }
    reporter.done("Proxy", settings.bind);
    Ok((shared, version))
}

async fn abandon(error: io::Error, shared: Shared, reporter: &Reporter) -> io::Error {
    if let Err(error) = shared.stop(reporter).await {
        tracing::error!(%error, "service shutdown failed");
    }
    error
}

/// Names `apps/arena/destinations/standard` as `arena/standard`.
fn destinations(manifest: Option<&chunk_contract::DestinationManifest>) -> Vec<Destination> {
    manifest
        .into_iter()
        .flat_map(|manifest| &manifest.entries)
        .map(|(id, policy)| Destination {
            name: id.strip_prefix("apps/").unwrap_or(id).replacen("/destinations/", "/", 1),
            demand: SessionDemand {
                key: policy.destination.key.clone(),
                session_type: policy.destination.session_type.clone(),
                machine_profile: policy.destination.machine_profile.clone(),
            },
        })
        .collect()
}

/// The nodes and players control reports, or an error while it is unreachable.
pub(super) async fn observe(connection: &ControlConnection) -> io::Result<(Vec<NodeStatus>, Vec<PlayerStatus>)> {
    let request = async {
        let mut client = crate::players::client(connection).await?;
        let nodes = client.nodes(crate::players::auth(NodesRequest {}, &connection.token)?);
        let mut nodes = nodes.await.map_err(io::Error::other)?.into_inner().nodes;
        // Stopped nodes stay listed for their logs, below the running ones.
        nodes.sort_by_key(|node| node.phase == NodePhase::Stopped as i32);
        let players = client.players(crate::players::auth(PlayersRequest {}, &connection.token)?);
        Ok((nodes, players.await.map_err(io::Error::other)?.into_inner().players))
    };
    tokio::time::timeout(Duration::from_secs(2), request).await.map_err(io::Error::other)?
}

/// Queues a move of `player` to `demand` on their existing connection.
pub(super) async fn move_player(
    connection: &ControlConnection,
    player: String,
    demand: SessionDemand,
) -> io::Result<()> {
    let mut client = crate::players::client(connection).await?;
    let request = MovePlayerRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        player_id: player,
        demand: Some(demand),
        expected_source: None,
        expected_connection_id: String::new(),
    };
    client
        .move_player(crate::players::auth(request, &connection.token)?)
        .await
        .map_err(|status| io::Error::other(status.message().to_owned()))?;
    Ok(())
}

#[cfg(test)]
mod tests;
