//! Owns player sockets, serves Java Edition status, and authenticates online-mode login.

#[cfg(feature = "mc-26-1")]
mod server;
#[cfg(feature = "mc-26-1")]
pub use server::Proxy;

#[cfg(not(feature = "mc-26-1"))]
mod disabled;
#[cfg(not(feature = "mc-26-1"))]
pub use disabled::Proxy;

use std::{num::NonZeroUsize, time::Duration};

/// Limits and responses for the initial connection exchange.
#[derive(Debug, Clone)]
pub struct Config {
    pub motd: String,
    pub max_connections: NonZeroUsize,
    /// Deadline for the entire exchange, including writes; not reset by traffic.
    pub connection_timeout: Duration,
    /// Uncompressed packet size at which zlib is enabled. None disables compression.
    pub compression_threshold: Option<usize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            motd: "chunk — sessions coming soon".into(),
            max_connections: NonZeroUsize::new(1024).unwrap(),
            connection_timeout: Duration::from_secs(10),
            compression_threshold: Some(256),
        }
    }
}
