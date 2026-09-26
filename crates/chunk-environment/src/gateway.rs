use crate::Running;
use std::{io, net::SocketAddr, num::NonZeroUsize};
use tokio_util::sync::CancellationToken;

pub use chunk_proxy::PlatformTarget;

#[derive(Clone)]
pub struct GatewayConfig {
    pub bind: SocketAddr,
    pub motd: String,
    pub max_connections: NonZeroUsize,
}

impl GatewayConfig {
    /// Listens on `bind` with the proxy's default status message and connection limit.
    #[must_use]
    pub fn new(bind: SocketAddr) -> Self {
        let defaults = chunk_proxy::Config::default();
        Self { bind, motd: defaults.motd, max_connections: defaults.max_connections }
    }
}

/// The player listener, which admits and routes players through core.
pub struct Gateway {
    running: Running,
    retarget: Option<chunk_proxy::Retarget>,
}

impl Gateway {
    /// # Errors
    /// Reports proxy configuration and bind errors.
    pub async fn start(config: GatewayConfig, target: PlatformTarget) -> io::Result<Self> {
        let proxy = chunk_proxy::Proxy::bind(
            config.bind,
            chunk_proxy::Config {
                platform: Some(target),
                motd: config.motd,
                max_connections: config.max_connections,
                ..Default::default()
            },
        )
        .await?;
        let retarget = proxy.retarget();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(proxy.run(async move {
            shutdown.cancelled().await;
            Ok(())
        }));
        Ok(Self { running: Running { stop, task }, retarget })
    }

    /// Sends later player connections to `target`.
    /// # Errors
    /// Rejects core or backend endpoints the proxy cannot use.
    pub fn retarget(&self, target: PlatformTarget) -> io::Result<()> {
        self.retarget.as_ref().ok_or_else(|| io::Error::other("proxy is not running"))?.replace(target)
    }

    /// Whether the listener stopped.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.running.task.is_finished()
    }

    /// Closes the listener and every player connection.
    /// # Errors
    /// Reports listener and shutdown errors.
    pub async fn stop(self) -> io::Result<()> {
        self.running.stop().await
    }
}
