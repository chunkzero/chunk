mod entities;
pub(crate) mod feed;
mod store;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use chunk_proto::v1::{ClaimIdentity, ClaimRequest};
use sha2::{Digest, Sha256};

use crate::{Config, Error, Release, Result};
pub use entities::Generation;
use entities::Stamp;
pub(crate) use entities::{
    Capacity, Claim, Drain, HostState, Meta, MoveFailure, MoveIntent, Phase, PlayerState, ReleaseState, Roster,
    SessionState,
};

#[derive(Clone, Default)]
pub(crate) struct State {
    pub config: Vec<u8>,
    /// The release new placements use.
    pub current: Option<String>,
    pub releases: BTreeMap<String, ReleaseState>,
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

    /// The release `host` runs.
    pub fn host_release(&self, host: &str) -> Result<&Arc<Release>> {
        let name = &self.hosts.get(host).ok_or(Error::Invalid("unknown host"))?.release;
        Ok(&self.releases.get(name).ok_or(Error::Invalid("unknown release"))?.release)
    }

    /// The release that places `request`: for a login, the one its proxy routed it with, or the current one when it
    /// names none; for a move, its source's. Retired releases place nothing, and a login routed with one is
    /// rejected as unavailable so its proxy routes it again.
    pub fn placing(&self, request: &ClaimRequest) -> Result<(String, Arc<Release>)> {
        let name = match &request.source {
            None if request.deployment.is_empty() => {
                self.current.clone().ok_or(Error::Invalid("no current release"))?
            }
            None => {
                if self.releases.get(&request.deployment).is_none_or(|release| release.retired) {
                    return Err(Error::Unresolved(crate::ROUTE_AGAIN));
                }
                request.deployment.clone()
            }
            Some(source) => {
                let claim = self.claims.get(&source.operation_id).ok_or(Error::Invalid("missing move source"))?;
                let session = self.sessions.get(&claim.session).ok_or(Error::Invalid("missing move source"))?;
                self.hosts.get(&session.host).ok_or(Error::Invalid("missing move source"))?.release.clone()
            }
        };
        let release = self.releases.get(&name).ok_or(Error::Invalid("unknown release"))?;
        if release.retired {
            return Err(Error::Invalid("release retired"));
        }
        let release = release.release.clone();
        Ok((name, release))
    }

    /// Replaces [`Generation::PENDING`] with `generation`, the commit that applied the update.
    fn stamp(&mut self, generation: Generation) {
        self.claims.values_mut().for_each(|claim| claim.stamp(generation));
        self.moves.values_mut().for_each(|intent| intent.stamp(generation));
        entities::stamp_wire(&mut self.method_sequence, generation);
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
    /// The last committed state, with the claim changes that led to it.
    feed: feed::Feed,
    /// Runs once inside the next commit, before it is written, while other writers are excluded.
    #[cfg(test)]
    pub committing: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

struct Writable {
    store: store::Store,
    /// Set after a failed commit, whose outcome is unknown, until state is reloaded from storage.
    stale: bool,
}

impl Authority {
    /// Opens the environment's control state, first dropping every row when `fresh`.
    pub fn open(system: chunk_backend::System, config: &Config, fresh: bool) -> Result<Self> {
        let store = store::Store::new(system.clone())?;
        if fresh {
            store.drop_all()?;
        }
        let state = store.load()?;
        let feed = feed::Feed::new(state);
        let store = Mutex::new(Writable { store, stale: false });
        let authority = Self {
            system,
            store,
            feed,
            #[cfg(test)]
            committing: Mutex::default(),
        };
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
        self.feed.state()
    }

    pub fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.writer()?.update(change)
    }

    /// Excludes other writers until the returned guard drops.
    pub fn writer(&self) -> Result<Writer<'_>> {
        let store = self.store.lock().map_err(|_| Error::Unresolved("control authority poisoned"))?;
        Ok(Writer { authority: self, store })
    }

    /// Stops committing and releases the environment, so another authority can open it.
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
}

pub(crate) struct Writer<'a> {
    authority: &'a Authority,
    store: MutexGuard<'a, Writable>,
}

impl Writer<'_> {
    /// Applies `change` to the current state and commits the rows it touched as one transaction. Rows that `change`
    /// gave [`Generation::PENDING`] get the `(epoch, revision)` of that commit.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.recover()?;
        let previous = self.authority.read()?;
        let mut next = State::clone(&previous);
        let result = change(&mut next)?;
        let writes = store::Store::writes(&previous, &next)?;
        if writes.is_empty() {
            return Ok(result);
        }
        let touched = feed::touched(&previous, &next, writes.keys().map(|(key, _)| key));
        #[cfg(test)]
        if let Some(committing) = self.authority.committing.lock().ok().and_then(|mut hook| hook.take()) {
            committing();
        }
        match self.store.store.commit(next.epoch, writes) {
            Ok(revision) if revision > previous.revision => {
                next.revision = revision;
                let position = next.position();
                next.stamp(position);
                self.authority.feed.record(next, touched)?;
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
            self.authority.feed.reset(state)?;
            self.store.stale = false;
        }
        Ok(())
    }
}
