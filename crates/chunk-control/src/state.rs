mod store;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard, RwLock},
};

use chunk_proto::v1::{ClaimIdentity, ClaimPhase, ClaimRequest};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Config, Error, Result};

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
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Drain {
    pub request: Vec<u8>,
    pub host: String,
    pub deadline_ms: u64,
    /// Started by control itself, so no caller retries it and it can be forgotten once the host stops.
    pub automatic: bool,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoveIntent {
    pub request: Vec<u8>,
    pub canceled: bool,
    pub sequence: u64,
    pub failure: Option<MoveFailure>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct MoveFailure {
    pub reason: String,
    pub at_ms: u64,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct HostState {
    pub app: String,
    pub profile: String,
    pub retired: bool,
    pub idle_since_ms: Option<u64>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionState {
    pub empty_since_ms: Option<u64>,
    pub finish_requested: bool,
    pub finished: bool,
    pub host: String,
    pub session_type: String,
    pub demand_key: String,
    pub capacity: u32,
    pub configuration: serde_json::Value,
    pub retired: bool,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PlayerState {
    pub membership_generation: u64,
    pub delivery_generation: u64,
    pub current: Option<String>,
    pub pending: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Phase {
    Reserved,
    Activating,
    Attached,
    Arrived,
    Withdrawing,
    Released,
}

impl From<Phase> for ClaimPhase {
    fn from(phase: Phase) -> Self {
        match phase {
            Phase::Reserved => Self::Reserved,
            Phase::Activating => Self::Activating,
            Phase::Attached => Self::Attached,
            Phase::Arrived => Self::Arrived,
            Phase::Withdrawing => Self::Withdrawing,
            Phase::Released => Self::Released,
        }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct Claim {
    pub request: Vec<u8>,
    pub player: String,
    pub proxy: String,
    pub membership_generation: u64,
    pub delivery_generation: u64,
    pub session: String,
    pub phase: Phase,
    pub assignment: Option<Vec<u8>>,
    pub activated: bool,
    pub created_at_ms: u64,
    pub released_at_ms: Option<u64>,
}

impl Claim {
    pub fn identity(&self, operation: &str) -> ClaimIdentity {
        ClaimIdentity {
            operation_id: operation.into(),
            proxy_id: self.proxy.clone(),
            membership_generation: self.membership_generation,
            delivery_generation: self.delivery_generation,
        }
    }

    pub fn matches(&self, request: &ClaimRequest) -> Result<()> {
        if self.request != request.encode_to_vec() {
            return Err(Error::Invalid("claim operation changed"));
        }
        Ok(())
    }
}

impl State {
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
}

/// The durable control state. Reads share the last committed state; writers serialize on the store.
pub(crate) struct Authority {
    store: Mutex<Writable>,
    current: RwLock<Arc<State>>,
}

struct Writable {
    store: store::Store,
    /// Set after a failed commit, whose outcome is unknown, until state is reloaded from storage.
    stale: bool,
}

impl Authority {
    pub fn open(path: &Path, config: &Config) -> Result<Self> {
        let store = store::Store::open(path)?;
        let current = RwLock::new(Arc::new(store.load()?));
        let authority = Self { store: Mutex::new(Writable { store, stale: false }), current };
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
    /// Applies `change` to the current state and commits only the entities it touched.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.recover()?;
        let previous = self.authority.read()?;
        let mut next = State::clone(&previous);
        let result = change(&mut next)?;
        let changes = store::changes(&previous, &next)?;
        if changes.is_empty() {
            return Ok(result);
        }
        if let Err(error) = self.store.store.write(&changes) {
            self.store.stale = true;
            let _ = self.recover();
            return Err(error);
        }
        self.authority.publish(next)?;
        Ok(result)
    }

    fn recover(&mut self) -> Result<()> {
        if self.store.stale {
            self.authority.publish(self.store.store.load()?)?;
            self.store.stale = false;
        }
        Ok(())
    }
}
