//! The environment process: core and gateway services, selected by `CHUNK_SERVICES`.

mod core;
mod gateway;
mod managed;
mod services;

pub use self::core::{Core, CoreConfig};
pub use gateway::{Gateway, GatewayConfig, PlatformTarget};
pub use managed::ManagementConfig;
pub use services::{Service, Services};

use chunk_service::{optional, required};
use std::{io, sync::OnceLock, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const STARTUP: Duration = Duration::from_secs(30);

pub struct Config {
    pub services: Services,
    pub core: CoreConfig,
    pub gateway: GatewayConfig,
    /// Set when the management service deploys the environment; otherwise core serves its bundle.
    pub management: Option<ManagementConfig>,
}

impl Config {
    /// Reads `CHUNK_SERVICES`, `CHUNK_ENVIRONMENT_ID` (or `CHUNK_ENVIRONMENT`), `CHUNK_STATE`, `CHUNK_BACKEND_BIND`,
    /// `CHUNK_CONTROL_BIND`, and the gateway's `CHUNK_BIND`, `CHUNK_MOTD` and `CHUNK_MAX_CONNECTIONS`. With
    /// `CHUNK_MANAGEMENT_URL`, it also reads `CHUNK_ENVIRONMENT_TOKEN` and serves what management deploys; otherwise it
    /// serves `CHUNK_BUNDLE`. Connection records go to `$CHUNK_STATE/backend.json` and `$CHUNK_STATE/control.json`.
    /// # Errors
    /// Reports missing or invalid variables.
    pub fn from_env() -> io::Result<Self> {
        let services: Services =
            optional::<String>("CHUNK_SERVICES")?.map_or_else(|| Ok(Services::default()), |s| s.parse())?;
        if !services.contains(Service::Core) {
            return Err(gateway_only());
        }
        let state: std::path::PathBuf = required("CHUNK_STATE")?;
        let management = match optional("CHUNK_MANAGEMENT_URL")? {
            Some(url) => Some(ManagementConfig { url, token: required("CHUNK_ENVIRONMENT_TOKEN")? }),
            None => None,
        };
        let environment = match optional("CHUNK_ENVIRONMENT_ID")? {
            Some(environment) => environment,
            None => {
                optional("CHUNK_ENVIRONMENT")?.ok_or_else(|| io::Error::other("CHUNK_ENVIRONMENT_ID is required"))?
            }
        };
        let core = CoreConfig {
            bundle: if management.is_some() { None } else { Some(required("CHUNK_BUNDLE")?) },
            environment,
            backend_record: state.join("backend.json"),
            control_record: state.join("control.json"),
            state,
            backend_bind: optional("CHUNK_BACKEND_BIND")?.unwrap_or(([127, 0, 0, 1], 25568).into()),
            control_bind: optional("CHUNK_CONTROL_BIND")?.unwrap_or(([127, 0, 0, 1], 25567).into()),
            fresh: false,
        };
        let mut gateway = GatewayConfig::new(optional("CHUNK_BIND")?.unwrap_or(([0, 0, 0, 0], 25565).into()));
        gateway.motd = optional("CHUNK_MOTD")?.unwrap_or_else(|| "chunk".into());
        if let Some(max_connections) = optional("CHUNK_MAX_CONNECTIONS")? {
            gateway.max_connections = max_connections;
        }
        Ok(Self { services, core, gateway, management })
    }
}

fn gateway_only() -> io::Error {
    io::Error::other("CHUNK_SERVICES=gateway is not supported yet; run core,gateway or core")
}

/// Runs the selected services until `stop` or until one of them stops, then stops the gateway before core. Under
/// management, the gateway starts with the first deployment, and a core that management fences stops.
/// # Errors
/// Reports a layout without core, which is not supported yet, startup errors, a service that stopped on its own, a
/// fenced core, and shutdown errors.
pub async fn run(config: Config, stop: CancellationToken) -> io::Result<()> {
    if !config.services.contains(Service::Core) {
        return Err(gateway_only());
    }
    let environment = config.core.environment.clone();
    let state = config.core.state.clone();
    let core = Core::start(config.core, |_| {}).await?;
    let gateway = OnceLock::new();
    let gateway_config = config.services.contains(Service::Gateway).then_some(config.gateway);
    let managed = if let Some(management) = config.management {
        Some(managed::Managed::new(management, environment, &state, &core, &gateway, gateway_config))
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
    let managed = async {
        match managed {
            Some(managed) => managed.run().await,
            None => std::future::pending().await,
        }
    };
    let failure = tokio::select! {
        () = stop.cancelled() => Ok(()),
        failure = failed => Err(io::Error::other(failure)),
        error = managed => Err(error),
    };
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
