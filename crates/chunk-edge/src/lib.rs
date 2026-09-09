//! Runs the edge player listener through `chunk-proxy`.
//! This entry point does not require proxy and backend process colocation.

use std::{future::Future, io, net::SocketAddr};

pub use chunk_proxy::{Config as ProxyConfig, GameplayTarget, PlatformTarget};

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
