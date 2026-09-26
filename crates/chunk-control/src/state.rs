mod entities;
pub(crate) mod feed;
mod store;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard, RwLock},
};

use chunk_proto::v1::ClaimIdentity;
use sha2::{Digest, Sha256};

use crate::{Config, Error, Result};
pub use entities::Generation;
pub(crate) use entities::{
    Capacity, Claim, Drain, HostState, Meta, MoveFailure, MoveIntent, Phase, PlayerState, Roster, SessionState,
};
pub use store::clear;

#[derive(Clone, Default)]
pub(crate) struct State {
    pub config: Vec<u8>,
    pub hosts: BTreeMap<String, HostState>,
    pub sessions: BTreeMap<String, SessionState>,
    pub players: BTreeMap<String, PlayerState>,
    pub claims: BTreeMap<String, Claim>,
    pub method_sequence: u64,
    pub moves: BTreeMap<String, MoveIntent>,
    pub drains: BTreeMap<String, Drain>,
    pub rosters: BTreeMap<String, Roster>,
    /// The store epoch, fixed while control runs.
    pub epoch: u64,
    /// The revision of the last commit this state includes. App commits share the log, so later revisions may
    /// belong to them.
    pub revision: u64,
}

impl State {
    /// A generation after every earlier control commit and no later than the commit that will apply the current
    /// update, which may follow app commits.
    pub fn next_generation(&self) -> Result<Generation> {
        Generation::new(self.epoch, self.revision + 1)
    }

    pub fn position(&self) -> Generation {
        Generation { epoch: self.epoch, revision: self.revision }
    }

    /// Whether `host`'s runtime is confirmed to have exited.
    pub fn released(&self, host: &str) -> bool {
        self.hosts.get(host).is_some_and(|host| host.capacity == Capacity::Released)
    }

    /// The arrived claim `identity` names, if it is still current and owns its player.
    pub fn arrived_claim(&self, identity: &ClaimIdentity) -> Option<&Claim> {
        let operation = &identity.operation_id;
        self.claims.get(operation).filter(|claim| {
            claim.identity(operation) == *identity
                && claim.phase == Phase::Arrived
                && self.players.get(&claim.player).and_then(|owner| owner.current.as_ref()) == Some(operation)
        })
    }

    /// Clears `operation` from its player's ownership, forgetting players that no longer own a claim.
    pub fn disown(&mut self, player: &str, operation: &str) {
        if let Some(owner) = self.players.get_mut(player) {
            if owner.current.as_deref() == Some(operation) {
                owner.current = None;
            }
            if owner.pending.as_deref() == Some(operation) {
                owner.pending = None;
            }
            if owner.current.is_none() && owner.pending.is_none() {
                self.players.remove(player);
            }
        }
    }
}

/// The durable control state. Reads share the last committed state; writers serialize here, then commit through the
/// environment's system lane.
pub(crate) struct Authority {
    system: chunk_backend::System,
    store: Mutex<Writable>,
    current: RwLock<Arc<State>>,
    feed: feed::Feed,
}

struct Writable {
    store: store::Store,
    /// Set after a failed commit, whose outcome is unknown, until state is reloaded from storage.
    stale: bool,
}

impl Authority {
    pub fn open(system: chunk_backend::System, config: &Config) -> Result<Self> {
        let store = store::Store::new(system.clone(), &config.deployment.deployment)?;
        let state = store.load()?;
        let feed = feed::Feed::new(state.position());
        let store = Mutex::new(Writable { store, stale: false });
        let authority = Self { system, store, current: RwLock::new(Arc::new(state)), feed };
        let fingerprint = Sha256::digest(serde_json::to_vec(config)?).to_vec();
        authority.update(|state| {
            if state.config.is_empty() {
                state.config.clone_from(&fingerprint);
            }
            if state.config != fingerprint {
                return Err(Error::Invalid("control configuration changed"));
            }
            Ok(())
        })?;
        Ok(authority)
    }

    pub fn read(&self) -> Result<Arc<State>> {
        Ok(self.current.read().map_err(|_| Error::Unresolved("control state poisoned"))?.clone())
    }

    pub fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.writer()?.update(change)
    }

    /// Excludes other writers until the returned guard drops.
    pub fn writer(&self) -> Result<Writer<'_>> {
        let store = self.store.lock().map_err(|_| Error::Unresolved("control authority poisoned"))?;
        Ok(Writer { authority: self, store })
    }

    /// Stops committing and releases this authority's scope, so another can open it.
    pub fn close(&self) -> Result<()> {
        self.store.lock().map_err(|_| Error::Unresolved("control authority poisoned"))?.store.close();
        Ok(())
    }

    pub fn feed(&self) -> &feed::Feed {
        &self.feed
    }

    /// Whether the environment store can no longer commit.
    pub fn stopped(&self) -> bool {
        self.system.stopped()
    }

    fn publish(&self, state: State) -> Result<()> {
        *self.current.write().map_err(|_| Error::Unresolved("control state poisoned"))? = Arc::new(state);
        Ok(())
    }
}

pub(crate) struct Writer<'a> {
    authority: &'a Authority,
    store: MutexGuard<'a, Writable>,
}

impl Writer<'_> {
    /// Applies `change` to the current state and commits the rows it touched as one transaction.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.recover()?;
        let previous = self.authority.read()?;
        let mut next = State::clone(&previous);
        let result = change(&mut next)?;
        let writes = self.store.store.writes(&previous, &next)?;
        if writes.is_empty() {
            return Ok(result);
        }
        let rows = feed::rows(&writes, self.store.store.scope());
        match self.store.store.commit(writes) {
            Ok(revision) if revision > previous.revision => {
                next.revision = revision;
                let position = next.position();
                self.authority.publish(next)?;
                self.authority.feed.record(position, rows);
                Ok(result)
            }
            outcome => {
                self.store.stale = true;
                let _ = self.recover();
                Err(outcome.err().unwrap_or(Error::Unresolved("control commit revision went back")))
            }
        }
    }

    fn recover(&mut self) -> Result<()> {
        if self.store.stale {
            let state = self.store.store.load()?;
            let position = state.position();
            self.authority.publish(state)?;
            self.authority.feed.reset(position);
            self.store.stale = false;
        }
        Ok(())
    }
}
