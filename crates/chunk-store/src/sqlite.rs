use std::{
    collections::BTreeMap,
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use crate::{Commit, Document, Error, Operation, Outcome, Result, Snapshot, Storage};

mod prepare;
mod revision;

use prepare::Prepared;

const MAX_DOCUMENT_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_DOCUMENTS: usize = 100_000;

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
        let (canonical, writer_lock) = acquire_writer_lock(path.as_ref())?;
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

fn acquire_writer_lock(path: &Path) -> Result<(PathBuf, File)> {
    // Create a fresh database file before canonicalizing its lock identity.
    File::options().create(true).truncate(false).write(true).open(path)?;
    let canonical = path.canonicalize()?;
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
    Ok((canonical, writer_lock))
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

fn outcome(connection: &Connection, operation: &Operation) -> Result<Option<Outcome>> {
    operation.validate()?;
    let record = connection
        .query_row(
            "SELECT fingerprint, revision, result FROM outcomes WHERE operation_id = ?1",
            [&operation.id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, crate::Revision>(1)?,
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
                revision,
                result: serde_json::from_str(&result)?,
            })
        })
        .transpose()
}

impl Storage for SqliteStore {
    fn snapshot(&mut self) -> Result<Snapshot> {
        let transaction = self.connection.transaction()?;
        let revision = revision::current(&transaction)?;
        let mut tables = BTreeMap::<_, BTreeMap<_, _>>::new();
        {
            let mut statement =
                transaction.prepare("SELECT table_name, id, revision, value FROM documents ORDER BY table_name, id")?;
            let mut rows = statement.query([])?;
            while let Some(row) = rows.next()? {
                let table: String = row.get(0)?;
                let id: String = row.get(1)?;
                let revision = row.get(2)?;
                let json: String = row.get(3)?;
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
        Ok(Snapshot::new(revision, tables))
    }

    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        outcome(&self.connection, operation)
    }

    fn commit(&mut self, commit: Commit) -> Result<Outcome> {
        let prepared = Prepared::new(&commit)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(outcome) = outcome(&transaction, &commit.operation)? {
            return Ok(outcome);
        }
        let current = revision::current(&transaction)?;
        if current != commit.expected {
            return Err(Error::Conflict {
                expected: commit.expected,
                actual: current,
            });
        }
        let next = revision::next(current)?;
        for (key, json) in prepared.writes {
            if let Some(json) = json {
                transaction.execute(
                    "INSERT INTO documents VALUES (?1, ?2, ?3, ?4)
                    ON CONFLICT (table_name, id) DO UPDATE SET revision = excluded.revision, value = excluded.value",
                    params![key.table, key.id, next, json],
                )?;
            } else {
                transaction.execute(
                    "DELETE FROM documents WHERE table_name = ?1 AND id = ?2",
                    params![key.table, key.id],
                )?;
            }
        }
        let (count, bytes): (usize, usize) = transaction.query_row(
            "SELECT count(*), coalesce(sum(length(CAST(value AS BLOB))), 0) FROM documents",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if count > MAX_DOCUMENTS || bytes > MAX_DOCUMENT_TOTAL_BYTES {
            return Err(Error::Capacity);
        }
        transaction.execute("UPDATE metadata SET revision = ?1 WHERE singleton = 1", [next])?;
        transaction.execute(
            "INSERT INTO outcomes VALUES (?1, ?2, ?3, ?4)",
            params![
                commit.operation.id,
                commit.operation.fingerprint.as_slice(),
                next,
                prepared.result
            ],
        )?;
        transaction.commit()?;
        Ok(Outcome {
            revision: next,
            result: commit.result,
        })
    }
}

#[cfg(test)]
mod tests;
