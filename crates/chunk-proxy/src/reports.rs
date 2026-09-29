//! What a gateway leaves for core to report to management: the latest server-list status for each hostname, which
//! edges answer pings with while the environment sleeps, and clients that failed authentication, which may then not
//! wake it.
// Without a protocol version there is no gateway to record anything.
#![cfg_attr(not(feature = "mc-26-2"), allow(dead_code))]

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, MutexGuard, PoisonError},
    time::SystemTime,
};

/// Hostnames kept, and the size of the status kept for each; together well within what management accepts.
const MAX_PINGS: usize = 16;
const MAX_STATUS_BYTES: usize = 16 * 1024;
/// Failed clients kept until taken.
const MAX_FAILURES: usize = 1000;

#[derive(Default)]
pub struct Reports(Mutex<State>);

#[derive(Default)]
struct State {
    /// Status JSON by normalised hostname, and when each was set, in the order `set` counts.
    pings: HashMap<String, (u64, String)>,
    set: u64,
    /// When each client last failed authentication.
    failures: HashMap<IpAddr, SystemTime>,
}

impl Reports {
    /// The latest status answered for each hostname, as `(hostname, status JSON)`.
    #[must_use]
    pub fn pings(&self) -> Vec<(String, String)> {
        let mut pings: Vec<_> =
            self.lock().pings.iter().map(|(host, (_, status))| (host.clone(), status.clone())).collect();
        pings.sort_unstable();
        pings
    }

    /// Takes the clients that failed authentication since the last call, with when each last failed.
    #[must_use]
    pub fn take_failed_auth(&self) -> Vec<(IpAddr, SystemTime)> {
        self.lock().failures.drain().collect()
    }

    /// Keeps `status` as the one answered for `host`, forgetting the hostname set longest ago once too many are kept.
    pub(crate) fn ping(&self, host: &str, status: &str) {
        let host = host.split('\0').next().unwrap_or_default();
        let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
        if host.is_empty() || status.len() > MAX_STATUS_BYTES {
            return;
        }
        let mut state = self.lock();
        state.set += 1;
        let set = state.set;
        state.pings.insert(host, (set, status.into()));
        if state.pings.len() > MAX_PINGS
            && let Some(oldest) = state.pings.iter().min_by_key(|(_, (set, _))| *set).map(|(host, _)| host.clone())
        {
            state.pings.remove(&oldest);
        }
    }

    /// Records that `client` failed authentication just now, unless too many others are waiting to be taken.
    pub(crate) fn failed_auth(&self, client: IpAddr) {
        let client = client.to_canonical();
        let mut state = self.lock();
        if state.failures.len() < MAX_FAILURES || state.failures.contains_key(&client) {
            state.failures.insert(client, SystemTime::now());
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_latest_status_per_hostname_within_bounds() {
        let reports = Reports::default();
        reports.ping("Play.Example.com.\0FML3\0", "{\"a\":1}");
        reports.ping("play.example.com", "{\"a\":2}");
        reports.ping("big.example.com", &"x".repeat(MAX_STATUS_BYTES + 1));
        assert_eq!(reports.pings(), [("play.example.com".into(), "{\"a\":2}".into())]);
        for index in 0..MAX_PINGS {
            reports.ping(&format!("{index}.example.com"), "{}");
        }
        assert_eq!(reports.pings().len(), MAX_PINGS);
        assert!(reports.pings().iter().all(|(host, _)| host != "play.example.com"), "the oldest goes");
    }

    #[test]
    fn hands_over_failed_clients_once() {
        let reports = Reports::default();
        reports.failed_auth("::ffff:203.0.113.7".parse().unwrap());
        reports.failed_auth("203.0.113.7".parse().unwrap());
        let failed: Vec<_> = reports.take_failed_auth().into_iter().map(|(client, _)| client).collect();
        assert_eq!(failed, ["203.0.113.7".parse::<IpAddr>().unwrap()]);
        assert!(reports.take_failed_auth().is_empty());
    }
}
