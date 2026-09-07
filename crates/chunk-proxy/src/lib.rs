//! Owns player sockets, serves status, authenticates login, and hosts a packet-simulated limbo.

#[cfg(feature = "mc-26-1")]
mod server;
#[cfg(feature = "mc-26-1")]
pub use server::Proxy;

#[cfg(not(feature = "mc-26-1"))]
mod disabled;
#[cfg(not(feature = "mc-26-1"))]
pub use disabled::Proxy;

use std::{num::NonZeroUsize, time::Duration};

/// Limits for login and configuration, and the server-list response.
#[derive(Debug, Clone)]
pub struct Config {
    /// Authenticated local gameplay bridge, when one is available.
    pub gameplay: Option<GameplayTarget>,
    pub motd: String,
    pub max_connections: NonZeroUsize,
    /// Deadline for the entire exchange, including writes; not reset by traffic.
    pub connection_timeout: Duration,
    /// Uncompressed packet size at which zlib is enabled. None disables compression.
    pub compression_threshold: Option<usize>,
    /// Deadline for configuration waiting and for the subsequent registry exchange.
    pub configuration_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            gameplay: None,
            motd: "chunk — sessions coming soon".into(),
            max_connections: NonZeroUsize::new(1024).unwrap(),
            connection_timeout: Duration::from_secs(10),
            compression_threshold: Some(256),
            configuration_timeout: Duration::from_secs(300),
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
