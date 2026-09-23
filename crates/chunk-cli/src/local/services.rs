use super::{Reporter, Settings, Staged, report::Destination};
use chunk_build::Release;
use chunk_contract::{BackendConnection, ControlConnection};
use chunk_proto::v1::{MovePlayerRequest, NodeStatus, NodesRequest, PlayerStatus, PlayersRequest, SessionDemand};
use std::{io, net::SocketAddr, sync::Arc, time::Duration};
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

/// The backend and proxy that outlive every release generation.
#[derive(Default)]
pub(super) struct Shared {
    backend: Option<Service>,
    handle: Option<chunk_backend::Backend>,
    connection: Option<BackendConnection>,
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

    /// Sends later player connections to `generation`.
    pub fn route(&self, generation: &Generation) -> io::Result<()> {
        let retarget = self.retarget.as_ref().ok_or_else(|| io::Error::other("proxy is not running"))?;
        retarget.replace(self.target(generation)?)
    }

    fn target(&self, generation: &Generation) -> io::Result<chunk_edge::PlatformTarget> {
        let control = generation.connection.clone().ok_or_else(|| io::Error::other("control is not running"))?;
        Ok(chunk_edge::PlatformTarget { backend: self.backend_connection(&generation.deployment)?, control })
    }

    pub fn failed(&self) -> bool {
        [&self.backend, &self.edge].into_iter().flatten().any(|service| service.task.is_finished())
    }

    /// Closes player connections first so releases can stop without new arrivals.
    pub async fn stop_proxy(&mut self) -> io::Result<()> {
        self.retarget = None;
        match self.edge.take() {
            Some(edge) => edge.stop().await.inspect_err(|error| tracing::error!(%error, "proxy shutdown failed")),
            None => Ok(()),
        }
    }

    pub async fn stop(mut self) -> io::Result<()> {
        let mut result = self.stop_proxy().await;
        // The service joins the backend engine only once this last outside handle is gone.
        if let Some(handle) = self.handle {
            tokio::task::spawn_blocking(move || drop(handle)).await.map_err(io::Error::other)?;
        }
        if let Some(backend) = self.backend
            && let Err(error) = backend.stop().await
        {
            result = Err(error);
        }
        result
    }
}

/// One deployment version's control authority and the JVMs it launched.
pub(super) struct Generation {
    pub release: Release,
    pub deployment: String,
    pub destinations: Vec<Destination>,
    control: Option<Service>,
    host: Option<Arc<chunk_control::ProcessHost>>,
    connection: Option<ControlConnection>,
}

impl Generation {
    /// Starts control for `staged`, whose backend version must already be active; on error, anything started is stopped again.
    pub async fn start(settings: &Settings, shared: &Shared, staged: Staged, bind: SocketAddr) -> io::Result<Self> {
        let mut generation = Self {
            release: staged.release,
            deployment: staged.bundle.id,
            destinations: destinations(staged.control.contracts.destinations.as_ref()),
            control: None,
            host: None,
            connection: None,
        };
        match generation.start_control(settings, shared, staged.control, &staged.java, bind).await {
            Ok(()) => Ok(generation),
            Err(error) => {
                if let Err(stop) = generation.stop().await {
                    tracing::error!(error = %stop, "control shutdown failed");
                }
                Err(error)
            }
        }
    }

    async fn start_control(
        &mut self,
        settings: &Settings,
        shared: &Shared,
        authority: chunk_control::Config,
        java: &std::path::Path,
        bind: SocketAddr,
    ) -> io::Result<()> {
        let state = self.state(settings);
        let host = Arc::new(chunk_control::ProcessHost::new(chunk_control::ProcessHostConfig {
            distribution: self.release.directory.clone(),
            java: java.into(),
            directory: state.join("nodes"),
            deployment: authority.deployment.clone(),
            apps: authority.apps.clone(),
            profiles: authority.profiles.clone(),
            backend: shared.backend_connection(&self.deployment)?,
        }));
        self.host = Some(host.clone());
        let token = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let config = chunk_control::server::Config {
            connection: state.join("connection.json"),
            state,
            bind,
            control: authority,
            host,
        };
        self.control =
            Some(Service { task: tokio::spawn(chunk_control::server::run(config, ready, token.clone())), stop: token });
        self.connection = Some(Service::ready(&mut self.control, started, "control").await?);
        Ok(())
    }

    fn state(&self, settings: &Settings) -> std::path::PathBuf {
        settings.state.join("control").join(&self.deployment)
    }

    pub fn connection(&self) -> Option<&ControlConnection> {
        self.connection.as_ref()
    }

    pub fn failed(&self) -> bool {
        self.control.as_ref().is_some_and(|service| service.task.is_finished())
    }

    /// Stops control, which asks each JVM to stop before terminating it.
    pub async fn stop(self) -> io::Result<()> {
        let mut result = Ok(());
        if let Some(control) = self.control
            && let Err(error) = control.stop().await
        {
            tracing::error!(%error, "control shutdown failed");
            result = Err(error);
        }
        if let Some(host) = self.host
            && let Err(error) = host.shutdown().await
        {
            result = Err(io::Error::other(error));
        }
        result
    }
}

/// Starts the backend, the first generation and the proxy; on error, everything started is stopped.
pub(super) async fn start(
    settings: &Settings,
    staged: Staged,
    reporter: &Reporter,
) -> io::Result<(Shared, Generation)> {
    let mut shared = Shared::default();
    reporter.running("Backend", settings.backend_bind);
    if let Err(error) = shared.start_backend(settings, &staged, reporter).await {
        return Err(abandon(error, shared, None).await);
    }
    reporter.running("Control", settings.control_bind);
    let generation = match Generation::start(settings, &shared, staged, settings.control_bind).await {
        Ok(generation) => generation,
        Err(error) => return Err(abandon(error, shared, None).await),
    };
    if let Some(connection) = generation.connection() {
        reporter.done("Control", &connection.endpoint);
    }
    reporter.running("Proxy", settings.bind);
    let proxy = match shared.target(&generation) {
        Ok(target) => shared.start_proxy(settings, target).await,
        Err(error) => Err(error),
    };
    if let Err(error) = proxy {
        return Err(abandon(error, shared, Some(generation)).await);
    }
    reporter.done("Proxy", settings.bind);
    Ok((shared, generation))
}

async fn abandon(error: io::Error, shared: Shared, generation: Option<Generation>) -> io::Error {
    if let Some(generation) = generation
        && let Err(error) = generation.stop().await
    {
        tracing::error!(%error, "control shutdown failed");
    }
    if let Err(error) = shared.stop().await {
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

/// The nodes and players a control authority reports, or an error while it is unreachable.
pub(super) async fn observe(connection: &ControlConnection) -> io::Result<(Vec<NodeStatus>, Vec<PlayerStatus>)> {
    let request = async {
        let mut client = crate::players::client(connection).await?;
        let nodes = client.nodes(crate::players::auth(NodesRequest {}, &connection.token)?);
        let nodes = nodes.await.map_err(io::Error::other)?.into_inner().nodes;
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
