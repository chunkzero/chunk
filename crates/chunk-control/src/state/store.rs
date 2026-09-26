use std::collections::BTreeMap;

use chunk_backend::{ScopeLock, System};
use chunk_store::{DatabaseSchema, DocumentKey, KeyRange, ReadBudget, Revision, Snapshot, Write};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{Capacity, Generation, Meta, Phase, State, entities::Stamp};
use crate::{Error, Result};

pub(crate) const CLAIMS: &str = "chunk_claims";
pub(crate) const MOVES: &str = "chunk_moves";
const META: &str = "chunk_control";
const HOSTS: &str = "chunk_hosts";
const SESSIONS: &str = "chunk_sessions";
const PLAYERS: &str = "chunk_players";
const DRAINS: &str = "chunk_drains";
const ROSTERS: &str = "chunk_rosters";
const TABLES: [&str; 8] = [META, HOSTS, SESSIONS, PLAYERS, CLAIMS, MOVES, DRAINS, ROSTERS];

/// Control state as system tables in the environment's store. Several control authorities, one per deployment
/// version, may share the tables, so each row ID starts with its authority's scope, which it holds exclusively.
pub(super) struct Store {
    system: System,
    scope: String,
    /// Held until [`Store::close`]; a closed store neither loads nor commits.
    lock: Option<ScopeLock>,
}

impl Store {
    pub fn new(system: System, deployment: &str) -> Result<Self> {
        let digest = format!("{:x}", Sha256::digest(deployment.as_bytes()));
        let scope = format!("{}/", &digest[..16]);
        let lock = system
            .lock_scope(&scope)
            .map_err(|_| Error::Invalid("another control authority already runs this deployment"))?;
        Ok(Self { system, scope, lock: Some(lock) })
    }

    /// Releases the scope for the next authority, even while tasks of this one still hold it.
    pub fn close(&mut self) {
        self.lock = None;
    }

    fn open(&self) -> Result<()> {
        if self.lock.is_none() {
            return Err(Error::Stopped);
        }
        Ok(())
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    /// Installs the system tables and loads this scope's rows. Control holds its whole state in memory and commits
    /// accept any size, so no read budget applies: a smaller one could stop control from reopening state it already
    /// accepted.
    pub fn load(&self) -> Result<State> {
        self.open()?;
        let snapshot = self.system.open(schema())?;
        let budget = &mut ReadBudget::new(usize::MAX, usize::MAX);
        let meta: Option<Meta> = snapshot
            .get_bounded(&self.key(META, "control")?, budget)?
            .map(|document| serde_json::from_value(document.value))
            .transpose()?;
        let meta = meta.unwrap_or_default();
        Ok(State {
            config: meta.config,
            method_sequence: meta.method_sequence,
            hosts: self.scan(&snapshot, HOSTS, budget)?,
            sessions: self.scan(&snapshot, SESSIONS, budget)?,
            players: self.scan(&snapshot, PLAYERS, budget)?,
            claims: self.scan(&snapshot, CLAIMS, budget)?,
            moves: self.scan(&snapshot, MOVES, budget)?,
            drains: self.scan(&snapshot, DRAINS, budget)?,
            rosters: self.scan(&snapshot, ROSTERS, budget)?,
            epoch: self.system.epoch().0,
            revision: snapshot.revision.0,
        })
    }

    /// Commits `writes` ahead of queued app commits, stamped with the commit's generation, returning its revision.
    pub fn commit(&self, epoch: u64, writes: Writes) -> Result<u64> {
        self.open()?;
        Ok(self.system.commit(move |revision| writes.stamp(epoch, revision))?.0)
    }

    /// The writes that turn `previous` into `next`.
    pub fn writes(&self, previous: &State, next: &State) -> Result<Writes> {
        let mut writes = Writes::default();
        if previous.config != next.config || previous.method_sequence != next.method_sequence {
            let meta = Meta { config: next.config.clone(), method_sequence: next.method_sequence };
            writes.put(self.key(META, "control")?, meta)?;
        }
        self.diff(HOSTS, &previous.hosts, &next.hosts, &mut writes)?;
        self.diff(SESSIONS, &previous.sessions, &next.sessions, &mut writes)?;
        self.diff(PLAYERS, &previous.players, &next.players, &mut writes)?;
        self.diff(CLAIMS, &previous.claims, &next.claims, &mut writes)?;
        self.diff(MOVES, &previous.moves, &next.moves, &mut writes)?;
        self.diff(DRAINS, &previous.drains, &next.drains, &mut writes)?;
        self.diff(ROSTERS, &previous.rosters, &next.rosters, &mut writes)?;
        Ok(writes)
    }

    fn key(&self, table: &str, id: &str) -> Result<DocumentKey> {
        Ok(DocumentKey::new(table, format!("{}{id}", self.scope))?)
    }

    fn scan<T: DeserializeOwned>(
        &self,
        snapshot: &Snapshot,
        table: &str,
        budget: &mut ReadBudget,
    ) -> Result<BTreeMap<String, T>> {
        // `0` follows `/`, so the range holds exactly the IDs that start with the scope.
        let end = format!("{}0", &self.scope[..self.scope.len() - 1]);
        let range = KeyRange { table: table.into(), start: Some(self.scope.clone()), end: Some(end) };
        let mut rows = BTreeMap::new();
        for (id, document) in snapshot.scan_bounded(&range, budget)? {
            let id = id.strip_prefix(&self.scope).unwrap_or(&id).to_owned();
            rows.insert(id, serde_json::from_value(document.value)?);
        }
        Ok(rows)
    }

    fn diff<T: Clone + PartialEq + Serialize + Stamp + Send + 'static>(
        &self,
        table: &str,
        previous: &BTreeMap<String, T>,
        next: &BTreeMap<String, T>,
        writes: &mut Writes,
    ) -> Result<()> {
        for id in previous.keys().filter(|id| !next.contains_key(*id)) {
            writes.ready.push(Write { key: self.key(table, id)?, value: None });
        }
        for (id, value) in next.iter().filter(|(id, value)| previous.get(*id) != Some(*value)) {
            writes.put(self.key(table, id)?, value.clone())?;
        }
        Ok(())
    }
}

/// Serializes a row that names [`Generation::PENDING`] once the commit's generation is known.
type Pending = Box<dyn FnOnce(Generation) -> serde_json::Result<Value> + Send>;

/// The writes of one update. Rows naming [`Generation::PENDING`] wait for their commit's revision, so the commit
/// thread serializes only those.
#[derive(Default)]
pub(super) struct Writes {
    ready: Vec<Write>,
    pending: Vec<(DocumentKey, Pending)>,
}

impl Writes {
    pub fn is_empty(&self) -> bool {
        self.ready.is_empty() && self.pending.is_empty()
    }

    /// Every written key, and whether the write removes it.
    pub fn keys(&self) -> impl Iterator<Item = (&DocumentKey, bool)> {
        let ready = self.ready.iter().map(|write| (&write.key, write.value.is_none()));
        ready.chain(self.pending.iter().map(|(key, _)| (key, false)))
    }

    fn put<T: Serialize + Stamp + Send + 'static>(&mut self, key: DocumentKey, mut row: T) -> Result<()> {
        if row.pending() {
            let stamp = move |generation| {
                row.stamp(generation);
                serde_json::to_value(row)
            };
            self.pending.push((key, Box::new(stamp)));
        } else {
            self.ready.push(Write { key, value: Some(serde_json::to_value(row)?) });
        }
        Ok(())
    }

    fn stamp(self, epoch: u64, revision: Revision) -> chunk_backend::Result<Vec<Write>> {
        let mut writes = self.ready;
        if self.pending.is_empty() {
            return Ok(writes);
        }
        let generation = Generation::new(epoch, revision.0).map_err(|_| chunk_store::Error::Capacity)?;
        for (key, row) in self.pending {
            writes.push(Write { key, value: Some(row(generation)?) });
        }
        Ok(writes)
    }
}

/// Deletes every control authority's rows, as when a local session starts over.
/// # Errors
/// Reports a stopped environment store or a failed commit.
pub fn clear(system: &System) -> Result<()> {
    let snapshot = system.open(schema())?;
    let budget = &mut ReadBudget::new(usize::MAX, usize::MAX);
    let mut writes = Vec::new();
    for table in TABLES {
        for (id, _) in snapshot.scan_bounded(&KeyRange { table: table.into(), start: None, end: None }, budget)? {
            writes.push(Write { key: DocumentKey::new(table, id)?, value: None });
        }
    }
    if !writes.is_empty() {
        system.commit(move |_| Ok(writes))?;
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
            "capacity?": {"type": "enum", "values": Capacity::NAMES}, "failure?": string,
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
