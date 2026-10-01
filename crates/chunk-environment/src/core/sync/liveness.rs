//! What each gateway last said about its connections through `chunk:active`, by gateway ID, so core never counts a
//! gateway it can't hear from as idle.
//!
//! A gateway whose stream ended may still hold players: its follower reconnects without closing their sockets. So it
//! counts as active until a new stream of the same gateway reports, or until core releases the gateway for good. A
//! gateway that never comes back and is never released keeps core from ever being idle, which is the safe outcome. The
//! gateway in core's own process is never released: it stops only with core, which then reports nothing more.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::time::Instant;

/// A live stream whose latest report is older than this counts as active. Gateways report every second.
const STALE: Duration = Duration::from_secs(3);

type Gateways = Arc<Mutex<HashMap<String, Gateway>>>;

#[derive(Default)]
struct Gateway {
    /// Its live `gateway/<id>` streams.
    streams: usize,
    /// The latest report's connections and time, since the gateway's latest stream opened.
    heard: Option<(u32, Instant)>,
    /// Core released the gateway for good.
    released: bool,
}

#[derive(Default)]
pub(crate) struct Liveness(Gateways);

impl Liveness {
    /// Counts a new stream of gateway `id` as live until the returned guard drops. The gateway counts as active until
    /// that stream reports.
    pub(super) fn open(&self, id: &str) -> Live {
        let mut gateways = lock(&self.0);
        let gateway = gateways.entry(id.to_owned()).or_default();
        gateway.streams += 1;
        gateway.heard = None;
        Live { gateways: self.0.clone(), id: id.to_owned() }
    }

    /// Records that gateway `id`, reporting on its current stream, holds `connections`.
    pub(super) fn heard(&self, id: &str, connections: u32) {
        if let Some(gateway) = lock(&self.0).get_mut(id).filter(|gateway| gateway.streams > 0) {
            gateway.heard = Some((connections, Instant::now()));
        }
    }

    /// Stops counting gateway `id` for good, once its machine is gone.
    pub(crate) fn release(&self, id: &str) {
        lock(&self.0).entry(id.to_owned()).or_default().released = true;
    }

    /// The connections unreleased gateways last reported on their live streams.
    pub(crate) fn connections(&self) -> u64 {
        let gateways = lock(&self.0);
        let heard = gateways.values().filter(|gateway| !gateway.released).filter_map(|gateway| gateway.heard);
        heard.map(|(connections, _)| u64::from(connections)).sum()
    }

    /// Whether a gateway may hold connections: its latest report counts some or is stale, or none came since its latest
    /// stream opened, including when no stream is live.
    pub(crate) fn active(&self) -> bool {
        let gateways = lock(&self.0);
        let mut unreleased = gateways.values().filter(|gateway| !gateway.released);
        unreleased.any(|gateway| gateway.heard.is_none_or(|(connections, at)| connections > 0 || at.elapsed() > STALE))
    }
}

/// A live stream, which stops counting once this drops. With no other live stream, its gateway is then unheard.
pub(super) struct Live {
    gateways: Gateways,
    id: String,
}

impl Drop for Live {
    fn drop(&mut self) {
        let mut gateways = lock(&self.gateways);
        let Some(gateway) = gateways.get_mut(&self.id) else { return };
        gateway.streams = gateway.streams.saturating_sub(1);
        if gateway.streams == 0 {
            gateway.heard = None;
        }
    }
}

fn lock(gateways: &Gateways) -> MutexGuard<'_, HashMap<String, Gateway>> {
    gateways.lock().unwrap_or_else(PoisonError::into_inner)
}
