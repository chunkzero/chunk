//! Flood limits on connections the edge hasn't handed to a gateway yet: those still sending a handshake, waiting for a
//! status answer, or held while their environment wakes. Once spliced, a connection counts against the gateway's
//! limits instead.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

/// How often refusals are logged at most.
const LOG_EVERY: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct Limits(Arc<Mutex<State>>);

struct State {
    total: usize,
    per_client: usize,
    pending: usize,
    /// Pending connections by client; a client with none has no entry, so this never outgrows `total`.
    clients: HashMap<IpAddr, usize>,
    /// Refusals since the last log.
    refused: u64,
    logged_at: Option<Instant>,
}

/// One admitted connection, until it is dropped.
pub(crate) struct Permit {
    state: Arc<Mutex<State>>,
    client: IpAddr,
}

impl Limits {
    /// At most `total` pending connections, and `per_client` from one client. IPv6 clients are counted by /64, since
    /// one client usually holds a whole /64.
    pub(crate) fn new(total: usize, per_client: usize) -> Self {
        let state = State { total, per_client, pending: 0, clients: HashMap::new(), refused: 0, logged_at: None };
        Self(Arc::new(Mutex::new(state)))
    }

    /// A permit for a connection from `address`, or None over either limit.
    pub(crate) fn admit(&self, address: IpAddr) -> Option<Permit> {
        let client = client(address);
        let mut state = lock(&self.0);
        let pending = state.clients.get(&client).copied().unwrap_or_default();
        if state.pending >= state.total || pending >= state.per_client {
            state.refused += 1;
            if state.logged_at.is_none_or(|at| at.elapsed() >= LOG_EVERY) {
                tracing::warn!(refused = state.refused, %client, "refusing connections over the pending limits");
                state.refused = 0;
                state.logged_at = Some(Instant::now());
            }
            return None;
        }
        state.pending += 1;
        state.clients.insert(client, pending + 1);
        Some(Permit { state: self.0.clone(), client })
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut state = lock(&self.state);
        state.pending -= 1;
        if let Some(pending) = state.clients.get_mut(&self.client) {
            *pending -= 1;
            if *pending == 0 {
                state.clients.remove(&self.client);
            }
        }
    }
}

fn client(address: IpAddr) -> IpAddr {
    match address.to_canonical() {
        IpAddr::V6(address) => IpAddr::V6((address.to_bits() & !u128::from(u64::MAX)).into()),
        address @ IpAddr::V4(_) => address,
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_each_client_and_the_total_and_forgets_idle_clients() {
        let limits = Limits::new(3, 2);
        let v4 = "203.0.113.7".parse().unwrap();
        let first = limits.admit(v4).unwrap();
        let second = limits.admit("::ffff:203.0.113.7".parse().unwrap()).expect("the same client, mapped");
        assert!(limits.admit(v4).is_none(), "over the per-client limit");
        let v6 = limits.admit("2001:db8::1".parse().unwrap()).unwrap();
        assert!(limits.admit("2001:db8::2".parse().unwrap()).is_none(), "over the total");
        drop(first);
        assert!(
            limits.admit("2001:db8::ffff:2".parse().unwrap()).is_some(),
            "room again once one closes, counted by /64"
        );

        drop(second);
        drop(v6);
        assert!(lock(&limits.0).clients.is_empty());
        assert_eq!(lock(&limits.0).pending, 0);
    }

    #[test]
    fn counts_ipv6_clients_by_64_prefix() {
        let limits = Limits::new(100, 2);
        let _held =
            ["2001:db8:0:1::1", "2001:db8:0:1:ffff::2"].map(|address| limits.admit(address.parse().unwrap()).unwrap());
        assert!(limits.admit("2001:db8:0:1:1234::3".parse().unwrap()).is_none(), "the same /64, over its limit");
        assert!(limits.admit("2001:db8:0:2::1".parse().unwrap()).is_some(), "another /64");
    }
}
