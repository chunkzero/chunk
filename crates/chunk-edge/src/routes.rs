//! The routing table, kept up to date from management's `WatchRoutes`.

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use chunk_management::{
    Client,
    v1::{Route, WatchRoutesRequest, WatchRoutesResponse},
};
use tokio::sync::watch;

use crate::status;

/// How long management has to start the stream, or to finish refusing it.
const ESTABLISH: Duration = Duration::from_secs(10);
/// Management sends a keepalive every 30 s; a stream silent for this long is dead.
const STALLED: Duration = Duration::from_secs(75);
const MIN_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Routes by normalised hostname. Cloning shares the table.
#[derive(Clone)]
pub(crate) struct Routes(Arc<Table>);

struct Table {
    entries: RwLock<HashMap<String, Entry>>,
    /// Sent after every update.
    changed: watch::Sender<()>,
    /// Whether management has sent the table at least once.
    loaded: AtomicBool,
}

/// A route, with the status cached for it while it is unchanged.
#[derive(Clone)]
pub(crate) struct Entry {
    pub route: Arc<Route>,
    pub status: Arc<status::Slot>,
}

impl Entry {
    /// The environment's ready gateways, empty while it sleeps or starts.
    pub(crate) fn gateways(&self) -> Vec<SocketAddr> {
        self.route.gateway_addresses.iter().filter_map(|address| address.parse().ok()).collect()
    }
}

impl Default for Routes {
    fn default() -> Self {
        Self(Arc::new(Table {
            entries: RwLock::default(),
            changed: watch::Sender::new(()),
            loaded: AtomicBool::new(false),
        }))
    }
}

impl Routes {
    /// The route of `hostname`, if it has one.
    pub(crate) fn get(&self, hostname: &str) -> Option<Entry> {
        self.0.entries.read().unwrap_or_else(PoisonError::into_inner).get(hostname).cloned()
    }

    /// Whether management has sent the table at least once; it stays true while management is unreachable.
    pub(crate) fn loaded(&self) -> bool {
        self.0.loaded.load(Ordering::Acquire)
    }

    /// Waits until `route`'s hostname lists gateways for the same environment, and returns them; None once it routes
    /// elsewhere or nowhere.
    pub(crate) async fn ready(&self, route: &Route) -> Option<Vec<SocketAddr>> {
        let mut changed = self.0.changed.subscribe();
        loop {
            let gateways = self.gateways(route)?;
            if !gateways.is_empty() {
                return Some(gateways);
            }
            changed.changed().await.ok()?;
        }
    }

    /// The gateways `route`'s hostname lists now for the same environment, or None if it routes elsewhere or nowhere.
    pub(crate) fn gateways(&self, route: &Route) -> Option<Vec<SocketAddr>> {
        let entry = self.get(&route.hostname)?;
        (entry.route.environment_id == route.environment_id).then(|| entry.gateways())
    }

    fn apply(&self, update: WatchRoutesResponse) {
        {
            let mut entries = self.0.entries.write().unwrap_or_else(PoisonError::into_inner);
            if update.reset {
                entries.clear();
            }
            for hostname in &update.removed_hostnames {
                entries.remove(hostname);
            }
            entries.extend(update.routes.into_iter().map(|route| {
                let entry = Entry { route: Arc::new(route), status: Arc::default() };
                (entry.route.hostname.clone(), entry)
            }));
        }
        self.0.changed.send_replace(());
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
    let mut stream = tokio::time::timeout(ESTABLISH, client.watch_routes(&WatchRoutesRequest {}))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the route stream did not start"))?
        .map_err(io::Error::other)?;
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
        let loaded = update.reset;
        routes.apply(update);
        if loaded {
            routes.0.loaded.store(true, Ordering::Release);
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn gives_up_on_a_stream_that_never_starts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::new(format!("http://{}", listener.local_addr().unwrap()));
        let _silent = tokio::spawn(async move { listener.accept().await });
        let error = follow(&client, &Routes::default(), &mut MIN_BACKOFF.clone()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }
}
