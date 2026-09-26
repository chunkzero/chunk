use super::{Reporter, Settings, Staged, report::Destination};
use chunk_build::Release;
use chunk_contract::ControlConnection;
use chunk_environment::{Core, CoreConfig, Gateway, GatewayConfig, PlatformTarget};
use chunk_proto::v1::{
    MovePlayerRequest, NodePhase, NodeStatus, NodesRequest, PlayerStatus, PlayersRequest, SessionDemand,
};
use std::{io, path::PathBuf, sync::Arc, time::Duration};

/// The backend, control and proxy that outlive every release.
pub(super) struct Shared {
    core: Core,
    gateway: Option<Gateway>,
}

impl Shared {
    pub fn backend(&self) -> Option<chunk_backend::Backend> {
        self.core.backend()
    }

    /// Control's endpoint, for the proxy and for `chunk players` and `chunk nodes`.
    pub fn control_connection(&self) -> io::Result<&ControlConnection> {
        self.core.control_connection()
    }

    pub fn control(&self) -> io::Result<Arc<chunk_control::Control>> {
        self.core.control()
    }

    /// Makes `bundle` resident beside earlier versions, retrying while the backend is busy.
    pub async fn deploy(&self, bundle: chunk_contract::Deployment) -> io::Result<()> {
        self.core.deploy(bundle).await
    }

    /// Makes `version` control's current release, whose JVMs launch from its release directory. Earlier releases keep
    /// their sessions.
    pub fn activate(&self, version: &Version) -> io::Result<()> {
        let distribution =
            chunk_control::Distribution { directory: version.release.directory.clone(), java: version.java.clone() };
        self.core.activate(&version.deployment, distribution, version.control.clone())
    }

    /// Sends later player connections to `version`'s backend.
    pub fn route(&self, version: &Version) -> io::Result<()> {
        let gateway = self.gateway.as_ref().ok_or_else(|| io::Error::other("proxy is not running"))?;
        gateway.retarget(self.target(version)?)
    }

    fn target(&self, version: &Version) -> io::Result<PlatformTarget> {
        let mut target = self.core.target()?;
        target.backend.deployment.clone_from(&version.deployment);
        Ok(target)
    }

    /// Whether the backend or proxy stopped.
    pub fn failed(&self) -> bool {
        self.core.failed() || self.gateway.as_ref().is_some_and(Gateway::failed)
    }

    pub fn control_failed(&self) -> bool {
        self.core.control_failed()
    }

    /// Closes player connections first so releases can stop without new arrivals.
    pub async fn stop_proxy(&mut self) -> io::Result<()> {
        match self.gateway.take() {
            Some(gateway) => gateway.stop().await.inspect_err(|error| tracing::error!(%error, "proxy shutdown failed")),
            None => Ok(()),
        }
    }

    /// Stops every JVM, then control, retrying until each JVM has confirmed its exit, however long that takes. The
    /// host and backend stay until then.
    pub async fn stop_control(&mut self, reporter: &Reporter) -> io::Result<()> {
        self.core.stop_control(waiting(reporter)).await
    }

    pub async fn stop(mut self, reporter: &Reporter) -> io::Result<()> {
        let mut result = self.stop_proxy().await;
        if let Err(error) = self.core.stop(waiting(reporter)).await {
            result = Err(error);
        }
        result
    }
}

/// Tells the user that stopping waits for the JVMs.
fn waiting(reporter: &Reporter) -> impl Fn() {
    move || reporter.running("Stop", "waiting for JVMs to stop")
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
    reporter.running("Backend", settings.backend_bind);
    let bundle = settings.state.join("deployment.json");
    std::fs::write(&bundle, serde_json::to_vec(&staged.bundle).map_err(io::Error::other)?)?;
    let config = CoreConfig {
        bundle,
        environment: staged.control.deployment.environment.clone(),
        state: settings.state.clone(),
        backend_record: settings.state.join("backend.json"),
        control_record: settings.state.join("control").join("connection.json"),
        backend_bind: settings.backend_bind,
        control_bind: settings.control_bind,
        // Starts over from an earlier session, first stopping any of its JVMs that still run.
        fresh: true,
    };
    let core = Core::start(config, |connection| {
        reporter.done("Backend", &connection.endpoint);
        reporter.running("Control", settings.control_bind);
    })
    .await?;
    let mut shared = Shared { core, gateway: None };
    let version = Version::new(staged);
    if let Err(error) = shared.activate(&version) {
        return Err(abandon(error, shared, reporter).await);
    }
    if let Ok(connection) = shared.control_connection() {
        reporter.done("Control", &connection.endpoint);
    }
    reporter.running("Proxy", settings.bind);
    let gateway = match shared.target(&version) {
        Ok(target) => Gateway::start(GatewayConfig::new(settings.bind), target).await,
        Err(error) => Err(error),
    };
    match gateway {
        Ok(gateway) => shared.gateway = Some(gateway),
        Err(error) => return Err(abandon(error, shared, reporter).await),
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
