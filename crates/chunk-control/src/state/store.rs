use std::{collections::BTreeMap, path::Path};

use chunk_store::{
    Commit, DatabaseSchema, DocumentKey, KeyRange, Operation, Revision, Snapshot, SqliteStore, Storage, Write,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use super::{Meta, Phase, State};
use crate::{Error, Result};

pub(crate) const CLAIMS: &str = "chunk_claims";
pub(crate) const MOVES: &str = "chunk_moves";
const META: &str = "chunk_control";
const HOSTS: &str = "chunk_hosts";
const SESSIONS: &str = "chunk_sessions";
const PLAYERS: &str = "chunk_players";
const DRAINS: &str = "chunk_drains";
const ROSTERS: &str = "chunk_rosters";
const META_ID: &str = "control";

/// The application ID of the entity-row format that preceded system tables.
const LEGACY_APPLICATION_ID: i32 = 0x6368_6b63;
/// Every control commit has a unique operation ID, so they share one request fingerprint.
const FINGERPRINT: [u8; 32] = *b"chunk-control-system-table-write";

/// Control state as system tables in an environment store.
pub(super) struct Store {
    store: SqliteStore,
}

impl Store {
    pub fn open(path: &Path, environment: &str) -> Result<Self> {
        if !path.exists() {
            crate::host::private_file(path)?;
        } else if rusqlite::Connection::open(path)?
            .pragma_query_value(None, "application_id", |row| row.get::<_, i32>(0))?
            == LEGACY_APPLICATION_ID
        {
            return Err(Error::Invalid("unsupported control state format"));
        }
        let mut store = SqliteStore::open(path, environment)?;
        store.apply_schema(&schema())?;
        Ok(Self { store })
    }

    pub fn load(&mut self) -> Result<State> {
        let snapshot = self.store.snapshot()?;
        let meta: Option<Meta> = snapshot
            .get(&DocumentKey::new(META, META_ID)?)?
            .map(|document| serde_json::from_value(document.value))
            .transpose()?;
        let meta = meta.unwrap_or_default();
        Ok(State {
            config: meta.config,
            method_sequence: meta.method_sequence,
            hosts: scan(&snapshot, HOSTS)?,
            sessions: scan(&snapshot, SESSIONS)?,
            players: scan(&snapshot, PLAYERS)?,
            claims: scan(&snapshot, CLAIMS)?,
            moves: scan(&snapshot, MOVES)?,
            drains: scan(&snapshot, DRAINS)?,
            rosters: scan(&snapshot, ROSTERS)?,
            epoch: self.store.epoch().0,
            revision: snapshot.revision.0,
        })
    }

    /// Commits `writes` on top of `previous`, returning the new revision.
    pub fn commit(&mut self, previous: &State, writes: Vec<Write>) -> Result<u64> {
        let revision = previous.revision + 1;
        let outcome = self.store.commit(Commit {
            expected: Revision(previous.revision),
            operation: Operation { id: format!("control/{}/{revision}", previous.epoch), fingerprint: FINGERPRINT },
            writes,
            result: Value::Null,
        })?;
        Ok(outcome.revision.0)
    }
}

fn scan<T: DeserializeOwned>(snapshot: &Snapshot, table: &str) -> Result<BTreeMap<String, T>> {
    let range = KeyRange { table: table.into(), start: None, end: None };
    let mut rows = BTreeMap::new();
    for (id, document) in snapshot.scan(&range)? {
        rows.insert(id, serde_json::from_value(document.value)?);
    }
    Ok(rows)
}

/// The writes that turn `previous` into `next`.
pub(super) fn writes(previous: &State, next: &State) -> Result<Vec<Write>> {
    let mut writes = Vec::new();
    if previous.config != next.config || previous.method_sequence != next.method_sequence {
        let meta = Meta { config: next.config.clone(), method_sequence: next.method_sequence };
        writes.push(Write { key: DocumentKey::new(META, META_ID)?, value: Some(serde_json::to_value(meta)?) });
    }
    diff(HOSTS, &previous.hosts, &next.hosts, &mut writes)?;
    diff(SESSIONS, &previous.sessions, &next.sessions, &mut writes)?;
    diff(PLAYERS, &previous.players, &next.players, &mut writes)?;
    diff(CLAIMS, &previous.claims, &next.claims, &mut writes)?;
    diff(MOVES, &previous.moves, &next.moves, &mut writes)?;
    diff(DRAINS, &previous.drains, &next.drains, &mut writes)?;
    diff(ROSTERS, &previous.rosters, &next.rosters, &mut writes)?;
    Ok(writes)
}

fn diff<T: PartialEq + Serialize>(
    table: &str,
    previous: &BTreeMap<String, T>,
    next: &BTreeMap<String, T>,
    writes: &mut Vec<Write>,
) -> Result<()> {
    for id in previous.keys().filter(|id| !next.contains_key(*id)) {
        writes.push(Write { key: DocumentKey::new(table, id.as_str())?, value: None });
    }
    for (id, value) in next.iter().filter(|(id, value)| previous.get(*id) != Some(*value)) {
        writes.push(Write { key: DocumentKey::new(table, id.as_str())?, value: Some(serde_json::to_value(value)?) });
    }
    Ok(())
}

/// Declares every system table. Later changes may only add optional fields, tables and indexes.
fn schema() -> DatabaseSchema {
    let string = json!({"type": "string"});
    let integer = json!({"type": "integer"});
    let boolean = json!({"type": "boolean"});
    let generation = json!({"type": "object", "fields": {
        "epoch": {"schema": integer}, "revision": {"schema": integer},
    }});
    let strings = json!({"type": "array", "items": string});
    let tables = json!({
        META: {"config": string, "method_sequence": integer},
        HOSTS: {
            "app": string, "profile": string, "retired": boolean, "idle_since_ms?": integer,
            "process?": {"type": "object", "fields": {
                "id": {"schema": string}, "generation": {"schema": integer}, "token_sha256": {"schema": string},
            }},
        },
        SESSIONS: {
            "empty_since_ms?": integer, "finish_requested": boolean, "finished": boolean, "host": string,
            "session_type": string, "demand_key": string, "capacity": integer, "configuration": string,
            "retired": boolean,
        },
        PLAYERS: {"current?": string, "pending?": string},
        CLAIMS: {
            "request": string, "player": string, "proxy": string, "membership": generation,
            "generation": generation, "session": string, "phase": {"type": "enum", "values": Phase::NAMES},
            "assignment?": string, "activated": boolean, "created_at_ms": integer, "released_at_ms?": integer,
            "roster?": string,
        },
        MOVES: {
            "request": string, "canceled": boolean, "sequence": integer,
            "failure?": {"type": "object", "fields": {"reason": {"schema": string}, "at_ms": {"schema": integer}}},
        },
        DRAINS: {"request": string, "host": string, "deadline_ms": integer, "automatic": boolean},
        ROSTERS: {"version": integer, "members": strings, "ready": strings, "admitted": boolean},
    });
    let tables = tables.as_object().into_iter().flatten().map(|(table, fields)| {
        let fields: serde_json::Map<_, _> = fields
            .as_object()
            .into_iter()
            .flatten()
            .map(|(name, schema)| {
                let (name, optional) = name.strip_suffix('?').map_or((name.as_str(), false), |name| (name, true));
                (name.to_owned(), json!({"schema": schema, "optional": optional}))
            })
            .collect();
        (table.clone(), json!({"fields": fields}))
    });
    serde_json::from_value(Value::Object(tables.collect())).expect("control system tables are a valid schema")
}

#[cfg(test)]
mod tests {
    #[test]
    fn system_tables_are_a_valid_schema() {
        chunk_contract::validate(&super::schema()).unwrap();
    }
}
