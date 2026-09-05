//! Owns player sockets, serves status, authenticates login, and hosts a packet-simulated limbo.

#[cfg(feature = "mc-26-1")]
mod server;
#[cfg(feature = "mc-26-1")]
pub use server::Proxy;

#[cfg(not(feature = "mc-26-1"))]
mod disabled;
#[cfg(not(feature = "mc-26-1"))]
pub use disabled::Proxy;

use std::{
    num::NonZeroUsize,
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};

/// Limits for login and configuration, and the server-list response.
#[derive(Debug, Clone)]
pub struct Config {
    pub motd: String,
    pub max_connections: NonZeroUsize,
    /// Deadline for the entire exchange, including writes; not reset by traffic.
    pub connection_timeout: Duration,
    /// Uncompressed packet size at which zlib is enabled. None disables compression.
    pub compression_threshold: Option<usize>,
    /// Deadline for configuration waiting and for the subsequent registry exchange.
    pub configuration_timeout: Duration,
    /// Open player sockets, kept current by the listener so operators can observe load.
    pub connections: Arc<AtomicUsize>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            motd: "chunk — sessions coming soon".into(),
            max_connections: NonZeroUsize::new(1024).unwrap(),
            connection_timeout: Duration::from_secs(10),
            compression_threshold: Some(256),
            configuration_timeout: Duration::from_secs(300),
            connections: Arc::default(),
        }
    }
}
