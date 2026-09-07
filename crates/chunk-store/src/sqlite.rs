use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{Connection, TransactionBehavior, params};

use crate::{Commit, DatabaseSchema, Error, Operation, Outcome, Result, Revision, Snapshot, Storage};

mod bootstrap;
mod codec;
mod read;
mod revision;
mod schema;
mod write;

/// Local single-writer adapter. The advisory lock lives beside the canonical
/// database path and is held for this object's lifetime. Every writer must use
/// this boundary; editing the database externally is unsupported.
pub struct SqliteStore {
    connection: Connection,
    path: PathBuf,
    schema: Arc<DatabaseSchema>,
    _writer_lock: File,
}

impl SqliteStore {
    /// Opens or initializes an environment database, retaining exclusive authority.
    /// Existing schema declarations are recovered from the migration journal.
    /// # Errors
    /// Fails if another backend is active, identity differs, format is unsupported,
    /// or the path/SQLite database cannot be opened. The legacy document format
    /// (`user_version` 1) requires an explicit schema-guided migration.
    pub fn open(path: impl AsRef<Path>, environment: &str) -> Result<Self> {
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("invalid environment identity"));
        }
        let (path, writer_lock) = bootstrap::acquire_writer_lock(path.as_ref())?;
        let connection = bootstrap::open(&path, environment)?;
        let schema = Arc::new(schema::load(&connection)?);
        Ok(Self {
            connection,
            path,
            schema,
            _writer_lock: writer_lock,
        })
    }
}

impl Storage for SqliteStore {
    fn apply_schema(&mut self, schema: &DatabaseSchema) -> Result<Revision> {
        let next_schema = schema::merge(&self.schema, schema)?;
        if next_schema == *self.schema {
            return revision::current(&self.connection);
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let next = revision::next(revision::current(&transaction)?)?;
        schema::apply(&transaction, &self.schema, &next_schema)?;
        transaction.execute(
            "INSERT INTO _chunk_migrations (revision, schema) VALUES (?1, ?2)",
            params![next, serde_json::to_string(&next_schema)?],
        )?;
        transaction.execute("UPDATE _chunk_metadata SET revision = ?1 WHERE singleton = 1", [next])?;
        transaction.commit()?;
        self.schema = Arc::new(next_schema);
        Ok(next)
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        read::snapshot(&self.path, self.schema.clone())
    }

    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        write::outcome(&self.connection, operation)
    }

    fn commit(&mut self, commit: Commit) -> Result<Outcome> {
        if let Some(outcome) = write::outcome(&self.connection, &commit.operation)? {
            return Ok(outcome);
        }
        let prepared = write::Prepared::new(&commit, &self.schema)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = revision::current(&transaction)?;
        if current != commit.expected {
            return Err(Error::Conflict {
                expected: commit.expected,
                actual: current,
            });
        }
        let next = revision::next(current)?;
        prepared.apply(&transaction, &self.schema, next)?;
        transaction.execute(
            "INSERT INTO _chunk_operations VALUES (?1, ?2, ?3, ?4)",
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
