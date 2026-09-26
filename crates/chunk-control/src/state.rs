mod entities;
pub(crate) mod feed;
mod store;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, RwLock},
};

use chunk_proto::v1::ClaimIdentity;
use sha2::{Digest, Sha256};

use crate::{Config, Error, Result};
pub use entities::Generation;
pub(crate) use entities::{Claim, Drain, HostState, Meta, MoveFailure, MoveIntent, Phase, PlayerState, SessionState};

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
    /// The store epoch, fixed while control runs.
    pub epoch: u64,
    /// The revision of the last commit this state includes.
    pub revision: u64,
}

impl State {
    /// The generation of the commit that will apply the current update.
    pub fn next_generation(&self) -> Result<Generation> {
        Generation::new(self.epoch, self.revision + 1)
    }

    pub fn position(&self) -> Generation {
        Generation { epoch: self.epoch, revision: self.revision }
    }

    pub fn retire_stopped_host(&mut self, id: &str) {
        if let Some(host) = self.hosts.get_mut(id) {
            host.retired = true;
        }
        for session in self.sessions.values_mut().filter(|session| session.host == id) {
            session.retired = true;
            session.finished = true;
        }
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

/// The durable control state. Reads share the last committed state; writers serialize on the store.
pub(crate) struct Authority {
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
    pub fn open(path: &Path, config: &Config) -> Result<Self> {
        let mut store = store::Store::open(path, &config.deployment.environment)?;
        let state = store.load()?;
        let feed = feed::Feed::new(state.position());
        let authority =
            Self { store: Mutex::new(Writable { store, stale: false }), current: RwLock::new(Arc::new(state)), feed };
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

    pub fn feed(&self) -> &feed::Feed {
        &self.feed
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
        let writes = store::writes(&previous, &next)?;
        if writes.is_empty() {
            return Ok(result);
        }
        let rows = feed::rows(&writes);
        match self.store.store.commit(&previous, writes) {
            Ok(revision) if revision == previous.revision + 1 => {
                next.revision = revision;
                let position = next.position();
                self.authority.publish(next)?;
                self.authority.feed.record(position, rows);
                Ok(result)
            }
            outcome => {
                self.store.stale = true;
                let _ = self.recover();
                Err(outcome.err().unwrap_or(Error::Unresolved("control commit revision skipped")))
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
