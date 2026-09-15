use std::{collections::BTreeMap, path::Path};

use chunk_proto::v1::{ClaimIdentity, ClaimRequest};
use chunk_store::{Commit, DocumentKey, Operation, SqliteStore, Storage, Write};
use prost::Message;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Config, Error, Result};

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct State {
    pub config: Vec<u8>,
    pub hosts: BTreeMap<String, HostState>,
    pub sessions: BTreeMap<String, SessionState>,
    pub players: BTreeMap<String, PlayerState>,
    pub claims: BTreeMap<String, Claim>,
    #[serde(default)]
    pub method_sequence: u64,
    #[serde(default)]
    pub moves: BTreeMap<String, MoveIntent>,
    #[serde(default)]
    pub drains: BTreeMap<String, Drain>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Drain {
    pub request: Vec<u8>,
    pub host: String,
    pub deadline_ms: u64,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct MoveIntent {
    pub request: Vec<u8>,
    pub canceled: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct HostState {
    pub app: String,
    pub profile: String,
    pub retired: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct SessionState {
    #[serde(default)]
    pub empty_since_ms: Option<u64>,
    #[serde(default)]
    pub finish_requested: bool,
    #[serde(default)]
    pub finished: bool,
    pub host: String,
    pub session_type: String,
    pub demand_key: String,
    pub capacity: u32,
    pub retired: bool,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct PlayerState {
    pub membership_generation: u64,
    pub delivery_generation: u64,
    pub current: Option<String>,
    #[serde(default)]
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

#[derive(Clone, Serialize, Deserialize)]
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

pub(crate) struct Authority {
    store: SqliteStore,
}

impl Authority {
    pub fn open(path: &Path, config: &Config) -> Result<Self> {
        if !path.exists() {
            super::host::private_file(path)?;
        }
        let mut store = SqliteStore::open(path, &format!("control:{}", config.deployment.environment))?;
        store.apply_schema(&serde_json::from_value(serde_json::json!({
            "control": {"fields": {"state": {"schema": {"type": "string"}}}}
        }))?)?;
        let mut authority = Self { store };
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

    pub fn read(&mut self) -> Result<State> {
        let snapshot = self.store.snapshot()?;
        snapshot
            .get(&DocumentKey::new("control", "state")?)?
            .map_or_else(|| Ok(State::default()), |doc| decode_state(&doc.value))
    }

    pub fn update<T>(&mut self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        // Reload after every boundary, including an ambiguous prior commit failure.
        let snapshot = self.store.snapshot()?;
        let key = DocumentKey::new("control", "state")?;
        let mut state = snapshot.get(&key)?.map_or_else(|| Ok(State::default()), |doc| decode_state(&doc.value))?;
        let result = change(&mut state)?;
        let value = serde_json::json!({"state": serde_json::to_string(&state)?});
        let fingerprint = Sha256::digest(serde_json::to_vec(&value)?).into();
        self.store.commit(Commit {
            expected: snapshot.revision,
            operation: Operation { id: uuid::Uuid::new_v4().to_string(), fingerprint },
            writes: vec![Write { key, value: Some(value) }],
            result: serde_json::Value::Null,
        })?;
        Ok(result)
    }
}

fn decode_state(value: &serde_json::Value) -> Result<State> {
    let json = value["state"].as_str().ok_or(Error::Invalid("corrupt control state"))?;
    Ok(serde_json::from_str(json)?)
}
