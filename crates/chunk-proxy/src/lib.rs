//! Owns player sockets, serves status, authenticates login, and hosts a packet-simulated limbo.

#[cfg(feature = "mc-26-2")]
mod command_tree;

#[cfg(feature = "mc-26-2")]
mod server;
#[cfg(feature = "mc-26-2")]
pub use server::{Proxy, Retarget};

#[cfg(feature = "bench-support")]
#[doc(hidden)]
pub use server::benchmark;

#[cfg(feature = "test-support")]
#[doc(hidden)]
pub use server::testing;

#[cfg(not(feature = "mc-26-2"))]
mod disabled;
mod trusted_edges;
#[cfg(not(feature = "mc-26-2"))]
pub use disabled::{Proxy, Retarget};
pub use trusted_edges::TrustedEdges;

use std::{
    num::NonZeroUsize,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

/// How many connections a proxy holds open, players and server-list pings alike.
#[derive(Clone, Debug, Default)]
pub struct Connections(Arc<AtomicUsize>);

impl Connections {
    #[must_use]
    pub fn open(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    #[cfg_attr(not(feature = "mc-26-2"), allow(dead_code))]
    fn set(&self, open: usize) {
        self.0.store(open, Ordering::Relaxed);
    }
}

/// Limits for login and configuration, and the server-list response.
#[derive(Debug, Clone)]
pub struct Config {
    /// Core's hooks, commands and durable session placement for managed gameplay.
    pub platform: Option<PlatformTarget>,
    pub motd: String,
    pub max_connections: NonZeroUsize,
    /// Deadline for the entire exchange, including writes; not reset by traffic.
    pub connection_timeout: Duration,
    /// Uncompressed packet size at which zlib is enabled. None disables compression.
    pub compression_threshold: Option<usize>,
    /// Deadline for configuration waiting and for the subsequent registry exchange.
    pub configuration_timeout: Duration,
    /// Accepts logins without encryption or Mojang verification, as vanilla offline mode does. Local testing only.
    pub offline_logins: bool,
    /// Edges whose connections name the player with a PROXY protocol v2 header.
    pub trusted_edges: TrustedEdges,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            platform: None,
            motd: "chunk — sessions coming soon".into(),
            max_connections: NonZeroUsize::new(1024).unwrap(),
            connection_timeout: Duration::from_secs(10),
            compression_threshold: Some(256),
            configuration_timeout: Duration::from_secs(300),
            offline_logins: false,
            trusted_edges: TrustedEdges::default(),
        }
    }
}

/// Core's endpoint, this gateway's identity in it, and the deployment whose hooks it runs.
#[derive(Clone)]
pub struct PlatformTarget {
    /// Core's endpoint, which serves the sync protocol.
    pub core: String,
    /// This gateway's identity in core; its credential authenticates every sync request.
    pub gateway: GatewayCredential,
    /// The deployment whose domain manifest and hooks route logins.
    pub deployment: String,
}

/// The ID core knows a gateway by, which names the claims it holds, and the credential core minted for it.
#[derive(Clone)]
pub struct GatewayCredential {
    pub id: String,
    pub credential: String,
}

impl std::fmt::Debug for PlatformTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlatformTarget")
            .field("core", &self.core)
            .field("gateway", &self.gateway.id)
            .field("deployment", &self.deployment)
            .finish_non_exhaustive()
    }
}
