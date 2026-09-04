//! Owns player sockets, serves Java Edition status, and rejects login.

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
    pub login_rejection: String,
    pub max_connections: NonZeroUsize,
    /// Deadline for the entire exchange, including writes; not reset by traffic.
    pub connection_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            motd: "chunk — sessions coming soon".into(),
            login_rejection: "This edge is running, but sessions are not available yet.".into(),
            max_connections: NonZeroUsize::new(1024).unwrap(),
            connection_timeout: Duration::from_secs(10),
        }
    }
}
