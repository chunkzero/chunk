//! The environment process: core and gateway services, selected by `CHUNK_SERVICES`.

mod core;
mod gateway;
mod managed;
mod services;

#[cfg(unix)]
pub use self::core::CommandLauncher;
pub use self::core::{
    Core, CoreConfig, LaunchSpec, Launcher, READINESS, RELEASE_TIMEOUT, ReleaseArchive, RunnerConfig,
};
pub use gateway::{Gateway, GatewayConfig, PlatformTarget, RemoteCore};
pub use managed::ManagementConfig;
pub use services::{Service, Services};

use chunk_service::{optional, required};
use std::{
    io,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

const STARTUP: Duration = Duration::from_secs(30);

pub enum Config {
    /// Core, with the in-process gateway when `gateway` is set.
    Core {
        core: Box<CoreConfig>,
        gateway: Option<GatewayConfig>,
        /// Set when the management service deploys the environment; otherwise core serves its bundle.
        management: Option<ManagementConfig>,
    },
    /// The gateway alone, joined to core on another machine.
    Gateway { gateway: GatewayConfig, core: RemoteCore },
}

impl Config {
    /// Reads `CHUNK_SERVICES`, `CHUNK_ENVIRONMENT_ID` (or `CHUNK_ENVIRONMENT`), and the gateway's `CHUNK_BIND`,
    /// `CHUNK_MOTD`, `CHUNK_MAX_CONNECTIONS`, `CHUNK_TRUSTED_EDGES` (comma-separated edge IPs or CIDRs) and
    /// `CHUNK_OFFLINE_LOGINS` (`1` admits unauthenticated players under any name; insecure, for tests only). With core,
    /// it also reads `CHUNK_STATE`, `CHUNK_CONTROL_BIND`, `CHUNK_CORE_BIND` and `CHUNK_PRIVATE_ADDRESS` (or
    /// `FLY_PRIVATE_IP`). With `CHUNK_MANAGEMENT_URL`, it also reads `CHUNK_ENVIRONMENT_TOKEN` and
    /// `CHUNK_SUSPEND_AFTER_SECONDS` and serves what management deploys; otherwise it serves `CHUNK_BUNDLE`. Control's
    /// connection record goes to `$CHUNK_STATE/control.json`.
    /// The gateway alone reads `CHUNK_CORE_ENDPOINT` and `CHUNK_GATEWAY_CREDENTIAL` instead.
    /// # Errors
    /// Reports missing or invalid variables.
    pub fn from_env() -> io::Result<Self> {
        let services: Services =
            optional::<String>("CHUNK_SERVICES")?.map_or_else(|| Ok(Services::default()), |s| s.parse())?;
        let environment = match optional("CHUNK_ENVIRONMENT_ID")? {
            Some(environment) => environment,
            None => {
                optional("CHUNK_ENVIRONMENT")?.ok_or_else(|| io::Error::other("CHUNK_ENVIRONMENT_ID is required"))?
            }
        };
        let mut gateway = GatewayConfig::new(optional("CHUNK_BIND")?.unwrap_or(([0, 0, 0, 0], 25565).into()));
        gateway.motd = optional("CHUNK_MOTD")?.unwrap_or_else(|| "chunk".into());
        if let Some(max_connections) = optional("CHUNK_MAX_CONNECTIONS")? {
            gateway.max_connections = max_connections;
        }
        if let Some(trusted_edges) = optional("CHUNK_TRUSTED_EDGES")? {
            gateway.trusted_edges = trusted_edges;
        }
        gateway.offline_logins = optional::<String>("CHUNK_OFFLINE_LOGINS")?.as_deref() == Some("1");
        if !services.contains(Service::Core) {
            let endpoint = required("CHUNK_CORE_ENDPOINT")?;
            let credential = required("CHUNK_GATEWAY_CREDENTIAL")?;
            return Ok(Self::Gateway { gateway, core: RemoteCore { endpoint, credential, environment } });
        }
        let state: std::path::PathBuf = required("CHUNK_STATE")?;
        let management = match optional("CHUNK_MANAGEMENT_URL")? {
            Some(url) => {
                let suspend_after = optional("CHUNK_SUSPEND_AFTER_SECONDS")?.map(Duration::from_secs);
                if suspend_after.is_some_and(|after| after.is_zero()) {
                    return Err(io::Error::other("CHUNK_SUSPEND_AFTER_SECONDS must be at least 1"));
                }
                Some(ManagementConfig { url, token: required("CHUNK_ENVIRONMENT_TOKEN")?, suspend_after })
            }
            None => None,
        };
        let core = CoreConfig {
            bundle: if management.is_some() { None } else { Some(required("CHUNK_BUNDLE")?) },
            environment,
            control_record: state.join("control.json"),
            state,
            control_bind: optional("CHUNK_CONTROL_BIND")?.unwrap_or(([127, 0, 0, 1], 25567).into()),
            core_bind: optional("CHUNK_CORE_BIND")?,
            private_address: match optional("CHUNK_PRIVATE_ADDRESS")? {
                Some(address) => Some(address),
                None => optional("FLY_PRIVATE_IP")?,
            },
            java: "java".into(),
            environment_token: management.as_ref().map(|management| management.token.clone()),
            fresh: false,
        };
        let gateway = services.contains(Service::Gateway).then_some(gateway);
        Ok(Self::Core { core: Box::new(core), gateway, management })
    }
}

/// Runs the configured services until `stop` or until one of them stops. With core, the gateway stops before core;
/// under management, the gateway starts with the first deployment, JVMs run on machines management provides, and a
/// core that management fences stops. The gateway alone follows core's current deployment and stops once core revokes
/// its credential.
/// # Errors
/// Reports startup errors, a service that stopped on its own, a fenced core, a gateway credential core rejects, and
/// shutdown errors.
pub async fn run(config: Config, stop: CancellationToken) -> io::Result<()> {
    match config {
        Config::Core { core, gateway, management } => run_core(*core, gateway, management, stop, RELEASE_TIMEOUT).await,
        Config::Gateway { gateway, core } => gateway::run_remote(core, gateway, stop, |_| {}).await,
    }
}

/// With management, machine stops still unconfirmed `release_bound` into shutdown are left to management.
async fn run_core(
    config: CoreConfig,
    gateway_config: Option<GatewayConfig>,
    management: Option<ManagementConfig>,
    stop: CancellationToken,
    release_bound: Duration,
) -> io::Result<()> {
    let environment = config.environment.clone();
    let state = config.state.clone();
    let management = management.map(|management| {
        let lease = watch::Sender::new(managed::Lease::Waiting);
        let launcher = Arc::new(managed::ManagementLauncher::new(management.client(), lease.subscribe()));
        (management, lease, launcher)
    });
    let launcher = management.as_ref().map(|(_, _, launcher)| launcher.clone());
    let core = match &launcher {
        Some(launcher) => Core::start_with_launcher(config, RunnerConfig::new(launcher.clone())).await?,
        None => Core::start(config, || {}).await?,
    };
    let gateway = OnceLock::new();
    let managed = if let Some((management, lease, _)) = management {
        Some(managed::Managed::new(&management, lease, environment, &state, &core, &gateway, gateway_config))
    } else {
        if let Some(gateway_config) = gateway_config {
            let started = match core.target() {
                Ok(target) => Gateway::start(gateway_config, target).await,
                Err(error) => Err(error),
            };
            match started {
                Ok(started) => _ = gateway.set(started),
                Err(error) => {
                    if let Err(error) = core.stop(|| {}).await {
                        tracing::error!(%error, "service shutdown failed");
                    }
                    return Err(error);
                }
            }
        }
        None
    };
    let stopping = managed.as_ref().map(managed::Managed::stopping).unwrap_or_default();
    tracing::info!("environment ready");
    let failed = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if core.failed() || gateway.get().is_some_and(Gateway::failed) {
                break "backend or gateway stopped";
            }
            if core.control_failed() {
                break "control stopped";
            }
        }
    };
    let mut managed = Box::pin(async {
        match managed {
            Some(managed) => managed.run().await,
            None => std::future::pending().await,
        }
    });
    let mut attached = true;
    let failure = tokio::select! {
        () = stop.cancelled() => Ok(()),
        failure = failed => Err(io::Error::other(failure)),
        error = &mut managed => {
            attached = false;
            Err(error)
        }
    };
    // Machines stop while the attach keeps the lease their releases carry current, though it activates nothing more and
    // starts no gateway. Management releases the machines of a core it superseded, so once `release_bound` passes the
    // rest are left to it.
    if let Some(launcher) = &launcher {
        stopping.cancel();
        if let Some(gateway) = gateway.get() {
            gateway.close();
        }
        let machines = core.stop_machines(release_bound);
        tokio::pin!(machines);
        let stopped = loop {
            tokio::select! {
                stopped = &mut machines => break stopped,
                error = &mut managed, if attached => {
                    tracing::warn!(%error, "management attach ended during shutdown");
                    attached = false;
                }
            }
        };
        if !stopped {
            launcher.abandon();
        }
    }
    drop(managed);
    let mut result = match gateway.into_inner() {
        Some(gateway) => gateway.stop().await.inspect_err(|error| tracing::error!(%error, "gateway shutdown failed")),
        None => Ok(()),
    };
    if let Err(error) = core.stop(|| {}).await {
        result = Err(error);
    }
    failure.and(result)
}

/// A service task and the token that stops it.
struct Running {
    stop: CancellationToken,
    task: JoinHandle<io::Result<()>>,
}

impl Running {
    /// Waits for the service in `slot` to report readiness, emptying `slot` if it stops first.
    async fn ready<T>(slot: &mut Option<Self>, started: oneshot::Receiver<T>, name: &str) -> io::Result<T> {
        match tokio::time::timeout(STARTUP, started).await {
            Ok(Ok(ready)) => return Ok(ready),
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

#[cfg(test)]
mod tests;
