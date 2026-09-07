use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::Path,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{Commit, Document, Error, Operation, Outcome, Result, Revision, Snapshot, Storage};

const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
const MAX_DATABASE_BYTES: i64 = 32 * 1024 * 1024;
const MAX_DOCUMENTS: i64 = 100_000;

/// Local single-writer adapter. The advisory lock lives beside the canonical
/// database path and is held for this object's lifetime. Every writer must use
/// this boundary; editing the database externally is unsupported.
pub struct SqliteStore {
    connection: Connection,
    _writer_lock: File,
}

impl SqliteStore {
    /// Opens or initializes an environment database, retaining exclusive authority.
    /// # Errors
    /// Fails if another backend is active, identity differs, schema is unsupported,
    /// or the path/SQLite database cannot be opened.
    pub fn open(path: impl AsRef<Path>, environment: &str) -> Result<Self> {
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("invalid environment identity"));
        }
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.as_ref())?;
        let canonical = path.as_ref().canonicalize()?;
        let mut lock_path = canonical.as_os_str().to_os_string();
        lock_path.push(".writer.lock");
        let writer_lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        writer_lock.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => Error::WriterLocked,
            std::fs::TryLockError::Error(error) => Error::Io(error),
        })?;
        let mut connection = Connection::open(canonical)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let schema: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match schema {
            0 => initialize(&mut connection, environment)?,
            1 => {}
            version => return Err(Error::SchemaVersion(version)),
        }
        let stored: String =
            connection.query_row("SELECT environment FROM metadata WHERE singleton = 1", [], |row| {
                row.get(0)
            })?;
        if stored != environment {
            return Err(Error::EnvironmentMismatch);
        }
        Ok(Self {
            connection,
            _writer_lock: writer_lock,
        })
    }
}

fn initialize(connection: &mut Connection, environment: &str) -> Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE metadata (
            singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
            environment TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision >= 0)
        );
        CREATE TABLE documents (
            table_name TEXT NOT NULL,
            id TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK (revision > 0),
            value TEXT NOT NULL,
            PRIMARY KEY (table_name, id)
        ) WITHOUT ROWID;
        CREATE TABLE outcomes (
            operation_id TEXT PRIMARY KEY,
            fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
            revision INTEGER NOT NULL UNIQUE CHECK (revision > 0),
            result TEXT NOT NULL
        );
        PRAGMA user_version = 1;",
    )?;
    transaction.execute("INSERT INTO metadata VALUES (1, ?1, 0)", [environment])?;
    transaction.commit()?;
    Ok(())
}

fn decode_revision(value: i64) -> Result<Revision> {
    Ok(Revision(
        u64::try_from(value).map_err(|_| Error::Invalid("negative stored revision"))?,
    ))
}

fn revision(connection: &Connection) -> Result<Revision> {
    let value: i64 = connection.query_row("SELECT revision FROM metadata WHERE singleton = 1", [], |row| {
        row.get(0)
    })?;
    decode_revision(value)
}

fn outcome(connection: &Connection, operation: &Operation) -> Result<Option<Outcome>> {
    operation.validate()?;
    let record = connection
        .query_row(
            "SELECT fingerprint, revision, result FROM outcomes WHERE operation_id = ?1",
            [&operation.id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    record
        .map(|(fingerprint, revision, result)| {
            if fingerprint != operation.fingerprint {
                return Err(Error::OperationMismatch);
            }
            Ok(Outcome {
                revision: decode_revision(revision)?,
                result: serde_json::from_str(&result)?,
            })
        })
        .transpose()
}

impl Storage for SqliteStore {
    fn snapshot(&mut self) -> Result<Snapshot> {
        let transaction = self.connection.transaction()?;
        let revision = revision(&transaction)?;
        let mut tables = BTreeMap::<_, BTreeMap<_, _>>::new();
        {
            let mut statement =
                transaction.prepare("SELECT table_name, id, revision, value FROM documents ORDER BY table_name, id")?;
            let mut rows = statement.query([])?;
            let mut bytes = 0;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                let id: String = row.get(1)?;
                let revision = decode_revision(row.get(2)?)?;
                let json: String = row.get(3)?;
                bytes += i64::try_from(json.len()).map_err(|_| Error::Capacity)?;
                count += 1;
                if bytes > MAX_DATABASE_BYTES || count > MAX_DOCUMENTS {
                    return Err(Error::Capacity);
                }
                tables.entry(table).or_default().insert(
                    id,
                    Document {
                        revision,
                        value: serde_json::from_str(&json)?,
                    },
                );
            }
        }
        transaction.commit()?;
        Ok(Snapshot { revision, tables })
    }

    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        outcome(&self.connection, operation)
    }

    fn commit(&mut self, commit: Commit) -> Result<Outcome> {
        commit.operation.validate()?;
        if commit.writes.len() > 256 {
            return Err(Error::Invalid("too many writes"));
        }
        let mut keys = BTreeSet::new();
        let writes = commit
            .writes
            .iter()
            .map(|write| {
                write.key.validate()?;
                if !keys.insert(&write.key) {
                    return Err(Error::Invalid("duplicate document write"));
                }
                let json = write.value.as_ref().map(serde_json::to_string).transpose()?;
                if json.as_ref().is_some_and(|value| value.len() > MAX_DOCUMENT_BYTES) {
                    return Err(Error::Capacity);
                }
                Ok((&write.key, json))
            })
            .collect::<Result<Vec<_>>>()?;
        let result = serde_json::to_string(&commit.result)?;
        if result.len() > MAX_DOCUMENT_BYTES {
            return Err(Error::Capacity);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = outcome(&transaction, &commit.operation)? {
            return Ok(outcome);
        }
        let current = revision(&transaction)?;
        if current != commit.expected {
            return Err(Error::Conflict {
                expected: commit.expected,
                actual: current,
            });
        }
        let next = current
            .0
            .checked_add(1)
            .filter(|value| *value <= i64::MAX.cast_unsigned())
            .ok_or(Error::Capacity)?;
        let next_sql = i64::try_from(next).map_err(|_| Error::Capacity)?;
        for (key, json) in writes {
            if let Some(json) = json {
                transaction.execute(
                    "INSERT INTO documents VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT (table_name, id) DO UPDATE SET revision = excluded.revision, value = excluded.value",
                    params![key.table, key.id, next_sql, json],
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM documents WHERE table_name = ?1 AND id = ?2",
                    params![key.table, key.id],
                )?;
            }
        }
        let (count, bytes): (i64, i64) = transaction.query_row(
            "SELECT count(*), coalesce(sum(length(CAST(value AS BLOB))), 0) FROM documents",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count > MAX_DOCUMENTS || bytes > MAX_DATABASE_BYTES {
            return Err(Error::Capacity);
        }
        transaction.execute("UPDATE metadata SET revision = ?1 WHERE singleton = 1", [next_sql])?;
        transaction.execute(
            "INSERT INTO outcomes VALUES (?1, ?2, ?3, ?4)",
            params![
                commit.operation.id,
                commit.operation.fingerprint.as_slice(),
                next_sql,
                result
            ],
        )?;
        transaction.commit()?;
        Ok(Outcome {
            revision: Revision(next),
            result: commit.result,
        })
    }
}

#[cfg(test)]
mod tests;
