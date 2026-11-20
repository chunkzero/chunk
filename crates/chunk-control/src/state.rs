mod entities;
pub(crate) mod feed;
mod store;

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, MutexGuard},
};

use chunk_proto::control::v1::{ClaimIdentity, ClaimRequest};
use chunk_store::DocumentKey;
use imbl::OrdMap;
use prost::Message;
use sha2::{Digest, Sha256};

use crate::{Config, Error, Release, Result};
use entities::Stamp;
pub(crate) use entities::{
    Capacity, Claim, Drain, HostState, Machine, Meta, MoveFailure, MoveIntent, OperatorCall, OperatorMethod, Phase,
    PlayerState, ReleaseDrain, ReleaseState, Roster, SessionState,
};
pub use entities::{Generation, Launch, MachineKind};

/// Its maps share structure with the states they were cloned from, so a clone is cheap and diffing it against its
/// origin visits only the rows that changed.
#[derive(Clone, Default)]
pub(crate) struct State {
    pub config: Vec<u8>,
    /// The release new placements use.
    pub current: Option<String>,
    /// The deployments asked to stop at once, rather than drain, until their backend versions are released.
    pub stopping: BTreeSet<String>,
    pub releases: OrdMap<String, ReleaseState>,
    pub hosts: OrdMap<String, HostState>,
    pub sessions: OrdMap<String, SessionState>,
    pub players: OrdMap<String, PlayerState>,
    pub claims: OrdMap<String, Claim>,
    pub method_sequence: u64,
    pub moves: OrdMap<String, MoveIntent>,
    /// The operation IDs of the moves out of each claim, by the claim's operation ID. Derived from `moves`, not stored.
    pub move_sources: OrdMap<String, BTreeSet<String>>,
    pub drains: OrdMap<String, Drain>,
    /// What each of the operator's operation IDs is bound to.
    pub operator_calls: OrdMap<String, OperatorCall>,
    pub rosters: OrdMap<String, Roster>,
    /// Machines core minted credentials for, by ID.
    pub machines: OrdMap<String, Machine>,
    /// What remote runners start on each host, by host ID.
    pub launches: OrdMap<String, Launch>,
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

    /// The release that places `request`, which is always the current one. A claim admitted in another release, as a
    /// login names in its request and a move in `approved`, is rejected as unavailable so its proxy admits it again.
    pub fn placing(&self, request: &ClaimRequest, approved: &str) -> Result<(String, Arc<Release>)> {
        let name = self.current.clone().ok_or(Error::Invalid("no current release"))?;
        let admitted = if request.source.is_none() { request.deployment.as_str() } else { approved };
        if !admitted.is_empty() && admitted != name {
            return Err(Error::Unresolved(crate::ROUTE_AGAIN));
        }
        let release = self.releases.get(&name).ok_or(Error::Invalid("unknown release"))?;
        Ok((name, release.release.clone()))
    }

    /// Re-indexes [`State::move_sources`] for the `written` moves, as they were in `previous` and are now.
    fn index_moves<'a>(&mut self, previous: &State, written: impl IntoIterator<Item = &'a str>) {
        let source = |intent: &MoveIntent| {
            ClaimRequest::decode(intent.request.as_slice()).ok()?.source.map(|source| source.operation_id)
        };
        for id in written {
            if let Some(from) = previous.moves.get(id).and_then(source)
                && let Some(ids) = self.move_sources.get_mut(&from)
            {
                ids.remove(id);
                if ids.is_empty() {
                    self.move_sources.remove(&from);
                }
            }
            if let Some(from) = self.moves.get(id).and_then(source) {
                self.move_sources.entry(from).or_default().insert(id.to_owned());
            }
        }
    }

    /// Replaces [`Generation::PENDING`] with `generation`, the commit that applied the update, in the `pending` rows:
    /// the only ones that can name it, since committed state never does.
    fn stamp<'a>(&mut self, generation: Generation, pending: impl IntoIterator<Item = &'a DocumentKey>) {
        for key in pending {
            if key.table == store::CLAIMS
                && let Some(claim) = self.claims.get_mut(&key.id)
            {
                claim.stamp(generation);
            } else if key.table == store::MOVES
                && let Some(intent) = self.moves.get_mut(&key.id)
            {
                intent.stamp(generation);
            }
        }
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

/// Applies `change` to the rows `select` accepts, copying only the nodes that hold them.
pub(crate) fn modify<T: Clone>(
    rows: &mut OrdMap<String, T>,
    mut select: impl FnMut(&str, &T) -> bool,
    mut change: impl FnMut(&str, &mut T),
) {
    let ids: Vec<_> = rows.iter().filter(|(id, row)| select(id, row)).map(|(id, _)| id.clone()).collect();
    for id in ids {
        if let Some(row) = rows.get_mut(&id) {
            change(&id, row);
        }
    }
}

/// The durable control state. Reads share the last committed state; writers serialize here, then commit through the
/// environment's system lane.
/// The control state `system` stores, read without serving it. Blocks on the store.
pub(crate) fn load(system: chunk_backend::System) -> Result<State> {
    store::Store::new(system)?.load()
}

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
        let moves = writes.keys().filter(|(key, _)| key.table == store::MOVES).map(|(key, _)| key.id.as_str());
        next.index_moves(&previous, moves);
        let pending: Vec<_> = writes.pending_keys().cloned().collect();
        #[cfg(test)]
        if let Some(committing) = self.authority.committing.lock().ok().and_then(|mut hook| hook.take()) {
            committing();
        }
        match self.store.store.commit(next.epoch, writes) {
            Ok(revision) if revision > previous.revision => {
                next.revision = revision;
                let position = next.position();
                next.stamp(position, &pending);
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
