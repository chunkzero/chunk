use std::{collections::BTreeMap, fs::File, path::Path, time::Duration};

use rusqlite::{Connection, params};
use serde::{Serialize, de::DeserializeOwned};

use super::State;
use crate::{Error, Result};

/// Marks a database written by this module; anything else in the file is foreign state.
const APPLICATION_ID: i32 = 0x6368_6b63;

/// One row per entity, so a commit writes only what an update changed.
pub(super) struct Store {
    connection: Connection,
    _writer_lock: File,
}

/// A row to upsert, or to delete when `value` is `None`.
pub(super) struct Change {
    kind: &'static str,
    id: String,
    value: Option<String>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.exists() {
            crate::host::private_file(path)?;
        }
        let mut lock_path = path.canonicalize()?.into_os_string();
        lock_path.push(".writer.lock");
        let writer_lock = File::options().create(true).truncate(false).read(true).write(true).open(lock_path)?;
        writer_lock.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => Error::Locked,
            std::fs::TryLockError::Error(error) => Error::Io(error),
        })?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let id: i32 = connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
        let tables: i64 = connection.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
        if id == 0 && tables == 0 {
            connection.execute_batch(&format!(
                "BEGIN;
                CREATE TABLE entities (
                    kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (kind, id)
                ) STRICT, WITHOUT ROWID;
                PRAGMA application_id = {APPLICATION_ID};
                COMMIT;"
            ))?;
        } else if id != APPLICATION_ID {
            return Err(Error::Invalid("unsupported control state format"));
        }
        Ok(Self { connection, _writer_lock: writer_lock })
    }

    pub fn load(&self) -> Result<State> {
        let mut state = State::default();
        let mut statement = self.connection.prepare("SELECT kind, id, value FROM entities")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let (kind, id, value): (String, String, String) = (row.get(0)?, row.get(1)?, row.get(2)?);
            match kind.as_str() {
                "config" => state.config = serde_json::from_str(&value)?,
                "method_sequence" => state.method_sequence = serde_json::from_str(&value)?,
                "host" => insert(&mut state.hosts, id, &value)?,
                "session" => insert(&mut state.sessions, id, &value)?,
                "player" => insert(&mut state.players, id, &value)?,
                "claim" => insert(&mut state.claims, id, &value)?,
                "move" => insert(&mut state.moves, id, &value)?,
                "drain" => insert(&mut state.drains, id, &value)?,
                _ => return Err(Error::Invalid("corrupt control state")),
            }
        }
        Ok(state)
    }

    pub fn write(&mut self, changes: &[Change]) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut upsert = transaction.prepare_cached("INSERT OR REPLACE INTO entities VALUES (?1, ?2, ?3)")?;
            let mut delete = transaction.prepare_cached("DELETE FROM entities WHERE kind = ?1 AND id = ?2")?;
            for change in changes {
                match &change.value {
                    Some(value) => upsert.execute(params![change.kind, change.id, value])?,
                    None => delete.execute(params![change.kind, change.id])?,
                };
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

fn insert<T: DeserializeOwned>(entities: &mut BTreeMap<String, T>, id: String, value: &str) -> Result<()> {
    entities.insert(id, serde_json::from_str(value)?);
    Ok(())
}

/// The rows that turn `previous` into `next`.
pub(super) fn changes(previous: &State, next: &State) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    if previous.config != next.config {
        changes.push(Change { kind: "config", id: String::new(), value: Some(serde_json::to_string(&next.config)?) });
    }
    if previous.method_sequence != next.method_sequence {
        let value = Some(serde_json::to_string(&next.method_sequence)?);
        changes.push(Change { kind: "method_sequence", id: String::new(), value });
    }
    diff("host", &previous.hosts, &next.hosts, &mut changes)?;
    diff("session", &previous.sessions, &next.sessions, &mut changes)?;
    diff("player", &previous.players, &next.players, &mut changes)?;
    diff("claim", &previous.claims, &next.claims, &mut changes)?;
    diff("move", &previous.moves, &next.moves, &mut changes)?;
    diff("drain", &previous.drains, &next.drains, &mut changes)?;
    Ok(changes)
}

fn diff<T: PartialEq + Serialize>(
    kind: &'static str,
    previous: &BTreeMap<String, T>,
    next: &BTreeMap<String, T>,
    changes: &mut Vec<Change>,
) -> Result<()> {
    for id in previous.keys().filter(|id| !next.contains_key(*id)) {
        changes.push(Change { kind, id: id.clone(), value: None });
    }
    for (id, value) in next.iter().filter(|(id, value)| previous.get(*id) != Some(*value)) {
        changes.push(Change { kind, id: id.clone(), value: Some(serde_json::to_string(value)?) });
    }
    Ok(())
}
