mod remote;

pub use remote::RemoteCore;
pub(crate) use remote::run as run_remote;

use crate::Running;
use std::{io, net::SocketAddr, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub use chunk_proxy::{PlatformTarget, Reports, TrustedEdges};

/// The longest login or configuration deadline, in seconds.
pub const MAX_TIMEOUT_SECONDS: u64 = 3600;

#[derive(Clone)]
pub struct GatewayConfig {
    pub bind: SocketAddr,
    pub motd: String,
    pub max_connections: NonZeroUsize,
    /// Deadline for a player's whole login exchange, authentication included.
    pub connection_timeout: Duration,
    /// Deadline for a player waiting in the configuration phase for a destination.
    pub configuration_timeout: Duration,
    /// Accepts unauthenticated logins, for local testing and smoke tests only.
    pub offline_logins: bool,
    /// Edges whose connections name the player with a PROXY protocol v2 header.
    pub trusted_edges: TrustedEdges,
}

impl GatewayConfig {
    /// Listens on `bind` with the proxy's default status message, connection limit and deadlines.
    #[must_use]
    pub fn new(bind: SocketAddr) -> Self {
        let defaults = chunk_proxy::Config::default();
        Self {
            bind,
            motd: defaults.motd,
            max_connections: defaults.max_connections,
            connection_timeout: defaults.connection_timeout,
            configuration_timeout: defaults.configuration_timeout,
            offline_logins: false,
            trusted_edges: defaults.trusted_edges,
        }
    }
}

/// The player listener, which admits and routes players through core.
pub struct Gateway {
    running: Running,
    retarget: Option<chunk_proxy::Retarget>,
    address: SocketAddr,
    reports: Arc<Reports>,
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
                connection_timeout: config.connection_timeout,
                configuration_timeout: config.configuration_timeout,
                offline_logins: config.offline_logins,
                trusted_edges: config.trusted_edges,
                ..Default::default()
            },
        )
        .await?;
        let address = proxy.local_addr()?;
        let retarget = proxy.retarget();
        let reports = proxy.reports();
        let stop = CancellationToken::new();
        let shutdown = stop.clone();
        let task = tokio::spawn(proxy.run(async move {
            shutdown.cancelled().await;
            Ok(())
        }));
        Ok(Self { running: Running { stop, task }, retarget, address, reports })
    }

    /// Where the listener accepts players.
    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
    }

    /// The statuses the listener answered and the clients that failed authentication at it.
    #[must_use]
    pub fn reports(&self) -> &Reports {
        &self.reports
    }

    /// Sends later player connections to `target`.
    /// # Errors
    /// Reports a proxy that isn't running.
    pub fn retarget(&self, target: PlatformTarget) -> io::Result<()> {
        self.retarget.as_ref().ok_or_else(|| io::Error::other("proxy is not running"))?.replace(target);
        Ok(())
    }

    /// Whether the listener stopped.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.running.task.is_finished()
    }

    /// Starts closing the listener and every player connection, which [`Self::stop`] waits for.
    pub(crate) fn close(&self) {
        self.running.stop.cancel();
    }

    /// Closes the listener and every player connection.
    /// # Errors
    /// Reports listener and shutdown errors.
    pub async fn stop(self) -> io::Result<()> {
        self.running.stop().await
    }
}
