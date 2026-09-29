//! What each gateway's live `gateway/<id>` stream last said about its connections through `chunk:active`, so core never
//! counts a gateway it can't hear from as idle.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::time::Instant;

/// A live stream whose latest report is older than this counts as active. Gateways report every second.
const STALE: Duration = Duration::from_secs(3);

/// The latest report's connections and time, by live stream; unset until the first.
type Streams = Arc<Mutex<HashMap<String, Option<(u32, Instant)>>>>;

#[derive(Default)]
pub(crate) struct Liveness(Streams);

impl Liveness {
    /// Counts `stream` as live until the returned guard drops.
    pub(super) fn open(&self, stream: &str) -> Live {
        lock(&self.0).insert(stream.to_owned(), None);
        Live { streams: self.0.clone(), stream: stream.to_owned() }
    }

    /// Records that the gateway of `stream`, if live, holds `connections`.
    pub(super) fn heard(&self, stream: &str, connections: u32) {
        if let Some(heard) = lock(&self.0).get_mut(stream) {
            *heard = Some((connections, Instant::now()));
        }
    }

    /// Whether a gateway may hold connections: a live stream's latest report counts some, is stale, or hasn't come.
    pub(crate) fn active(&self) -> bool {
        let streams = lock(&self.0);
        streams.values().any(|heard| heard.is_none_or(|(connections, at)| connections > 0 || at.elapsed() > STALE))
    }
}

/// A live stream, which stops counting once this drops.
pub(super) struct Live {
    streams: Streams,
    stream: String,
}

impl Drop for Live {
    fn drop(&mut self) {
        lock(&self.streams).remove(&self.stream);
    }
}

fn lock(streams: &Streams) -> MutexGuard<'_, HashMap<String, Option<(u32, Instant)>>> {
    streams.lock().unwrap_or_else(PoisonError::into_inner)
}
