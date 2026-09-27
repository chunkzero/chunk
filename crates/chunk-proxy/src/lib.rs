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
#[cfg(not(feature = "mc-26-2"))]
pub use disabled::{Proxy, Retarget};

use std::{num::NonZeroUsize, time::Duration};

/// Limits for login and configuration, and the server-list response.
#[derive(Debug, Clone)]
pub struct Config {
    /// Authenticated local gameplay bridge, when one is available.
    pub gameplay: Option<GameplayTarget>,
    /// Backend hooks and durable session placement for managed gameplay.
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gameplay: None,
            platform: None,
            motd: "chunk — sessions coming soon".into(),
            max_connections: NonZeroUsize::new(1024).unwrap(),
            connection_timeout: Duration::from_secs(10),
            compression_threshold: Some(256),
            configuration_timeout: Duration::from_secs(300),
            offline_logins: false,
        }
    }
}

/// Trusted gameplay destination; credentials are never included in diagnostics.
#[derive(Clone)]
pub struct GameplayTarget {
    pub endpoint: String,
    pub token: String,
    pub environment: String,
    pub deployment: String,
}

impl std::fmt::Debug for GameplayTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GameplayTarget")
            .field("endpoint", &self.endpoint)
            .field("environment", &self.environment)
            .field("deployment", &self.deployment)
            .finish_non_exhaustive()
    }
}

/// Core's endpoint and this gateway's identity in it, with the backend connection commands still use. Hooks run in
/// `backend.deployment`.
#[derive(Clone)]
pub struct PlatformTarget {
    /// Core's endpoint, which serves the sync protocol.
    pub core: String,
    /// This gateway's identity in core; its credential authenticates every sync request.
    pub gateway: GatewayCredential,
    pub backend: chunk_contract::BackendConnection,
    /// Control's credential, which command effects present on core's endpoint to move players and call session
    /// methods.
    pub control_token: String,
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
            .field("backend", &self.backend.endpoint)
            .finish_non_exhaustive()
    }
}
