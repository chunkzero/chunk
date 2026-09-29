//! The routing table, kept up to date from management's `WatchRoutes`.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, PoisonError, RwLock},
    time::Duration,
};

use chunk_management::{
    Client,
    v1::{Route, WatchRoutesRequest, WatchRoutesResponse},
};

/// Management sends a keepalive every 30 s; a stream silent for this long is dead.
const STALLED: Duration = Duration::from_secs(75);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Routes by normalised hostname. Cloning shares the table.
#[derive(Clone, Default)]
pub(crate) struct Routes(Arc<RwLock<HashMap<String, Route>>>);

impl Routes {
    /// The ready gateways of the environment `hostname` routes to, empty while it sleeps or starts; None for a hostname
    /// with no route.
    pub(crate) fn gateways(&self, hostname: &str) -> Option<Vec<SocketAddr>> {
        let routes = self.0.read().unwrap_or_else(PoisonError::into_inner);
        let route = routes.get(hostname)?;
        Some(route.gateway_addresses.iter().filter_map(|address| address.parse().ok()).collect())
    }

    fn apply(&self, update: WatchRoutesResponse) {
        let mut routes = self.0.write().unwrap_or_else(PoisonError::into_inner);
        if update.reset {
            routes.clear();
        }
        for hostname in &update.removed_hostnames {
            routes.remove(hostname);
        }
        routes.extend(update.routes.into_iter().map(|route| (route.hostname.clone(), route)));
    }
}

/// Follows `WatchRoutes` into `routes` for as long as it runs, reconnecting with backoff. The last table stays in use
/// while disconnected.
pub(crate) async fn watch(client: Client, routes: Routes) {
    let mut backoff = MIN_BACKOFF;
    loop {
        match follow(&client, &routes, &mut backoff).await {
            Ok(()) => tracing::warn!("route stream ended; reconnecting"),
            Err(error) => tracing::warn!(%error, "route stream failed; reconnecting"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

async fn follow(client: &Client, routes: &Routes, backoff: &mut Duration) -> io::Result<()> {
    let mut stream = client.watch_routes(&WatchRoutesRequest {}).await.map_err(io::Error::other)?;
    loop {
        let message = tokio::time::timeout(STALLED, stream.message())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "no message or keepalive"))?
            .map_err(io::Error::other)?;
        let Some(update) = message else { return Ok(()) };
        if update.reset {
            *backoff = MIN_BACKOFF;
            tracing::info!(routes = update.routes.len(), "routes loaded");
        }
        routes.apply(update);
    }
}
