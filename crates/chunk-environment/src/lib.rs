//! The environment process: core, gateway and exec services, selected by `CHUNK_SERVICES`.

mod core;
mod gateway;
mod services;

pub use self::core::{Core, CoreConfig};
pub use gateway::{Gateway, GatewayConfig, PlatformTarget};
pub use services::{Service, Services};

use chunk_service::{optional, required};
use std::{io, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tokio_util::sync::CancellationToken;

const STARTUP: Duration = Duration::from_secs(30);

pub struct Config {
    pub services: Services,
    pub core: CoreConfig,
    pub gateway: GatewayConfig,
}

impl Config {
    /// Reads `CHUNK_SERVICES`, `CHUNK_BUNDLE`, `CHUNK_ENVIRONMENT`, `CHUNK_STATE`, `CHUNK_BACKEND_BIND`,
    /// `CHUNK_CONTROL_BIND`, and the gateway's `CHUNK_BIND`, `CHUNK_MOTD` and `CHUNK_MAX_CONNECTIONS`. Connection
    /// records go to `$CHUNK_STATE/backend.json` and `$CHUNK_STATE/control.json`.
    /// # Errors
    /// Reports missing or invalid variables.
    pub fn from_env() -> io::Result<Self> {
        let services = optional::<String>("CHUNK_SERVICES")?.map_or_else(|| Ok(Services::default()), |s| s.parse())?;
        let state: std::path::PathBuf = required("CHUNK_STATE")?;
        let core = CoreConfig {
            bundle: required("CHUNK_BUNDLE")?,
            environment: required("CHUNK_ENVIRONMENT")?,
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
        Ok(Self { services, core, gateway })
    }
}

/// Runs the selected services until `stop` or until one of them stops, then stops the gateway before core.
/// # Errors
/// Reports startup errors, a service that stopped on its own, and shutdown errors.
pub async fn run(config: Config, stop: CancellationToken) -> io::Result<()> {
    let core = Core::start(config.core, |_| {}).await?;
    let gateway = if config.services.contains(Service::Gateway) {
        let started = match core.target() {
            Ok(target) => Gateway::start(config.gateway, target).await,
            Err(error) => Err(error),
        };
        match started {
            Ok(gateway) => Some(gateway),
            Err(error) => {
                if let Err(error) = core.stop(|| {}).await {
                    tracing::error!(%error, "service shutdown failed");
                }
                return Err(error);
            }
        }
    } else {
        None
    };
    tracing::info!("environment ready");
    let failed = async {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            if core.failed() || gateway.as_ref().is_some_and(Gateway::failed) {
                break "backend or gateway stopped";
            }
            if core.control_failed() {
                break "control stopped";
            }
        }
    };
    let failure = tokio::select! {
        () = stop.cancelled() => Ok(()),
        failure = failed => Err(io::Error::other(failure)),
    };
    let mut result = match gateway {
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
