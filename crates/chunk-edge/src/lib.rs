//! Runs the edge player listener through `chunk-proxy`.
//! Application hosting is not yet implemented.

use std::{future::Future, io, net::SocketAddr};

pub use chunk_proxy::Config as ProxyConfig;

/// Runs the edge's player listener.
///
/// # Errors
/// Returns proxy configuration, bind, listener or shutdown errors.
pub async fn run(
    address: SocketAddr,
    config: ProxyConfig,
    shutdown: impl Future<Output = io::Result<()>>,
) -> io::Result<()> {
    let proxy = chunk_proxy::Proxy::bind(address, config).await?;
    proxy.run(shutdown).await
}
