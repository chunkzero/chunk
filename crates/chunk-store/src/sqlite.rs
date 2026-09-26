use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use rusqlite::{Connection, params};

use crate::{
    Commit, DatabaseSchema, Epoch, Error, Operation, Outcome, Replication, Replicator, Result, RetryContext, Revision,
    Snapshot, Storage, replication,
};

pub(crate) mod bootstrap;
mod codec;
mod deployments;
mod jobs;
pub(crate) mod log;
mod operations;
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
    epoch: Epoch,
    log: Option<log::Log>,
    _writer_lock: bootstrap::WriterLock,
}

impl SqliteStore {
    /// Opens or initializes an environment database, retaining exclusive authority.
    /// Existing schema declarations are recovered from the migration journal, and
    /// older store formats are migrated in place.
    /// # Errors
    /// Fails if another backend is active, identity differs, format is unsupported,
    /// or the path/SQLite database cannot be opened. The legacy document format
    /// (`user_version` 1) requires an explicit schema-guided migration.
    pub fn open(path: impl AsRef<Path>, environment: &str) -> Result<Self> {
        validate_environment(environment)?;
        let (path, writer_lock) = bootstrap::acquire_writer_lock(path.as_ref())?;
        let (connection, _) = bootstrap::open(&path, environment)?;
        log::mark_unlogged(&connection)?;
        Self::new(connection, path, writer_lock, None)
    }

    /// Opens like [`Self::open`] and replicates every later write transaction to
    /// object storage. A database that does not exist yet is first restored from
    /// the latest snapshot and later log segments, under a new epoch.
    /// # Errors
    /// Fails like [`Self::open`], on object storage or restore failures, and with
    /// [`Error::StaleReplica`] when object storage holds a newer history. Writes
    /// fail with [`Error::Fenced`] once another store claims a newer epoch.
    pub fn open_replicated(
        path: impl AsRef<Path>,
        environment: &str,
        replication: Replication,
    ) -> Result<(Self, Replicator)> {
        validate_environment(environment)?;
        let (path, writer_lock) = bootstrap::acquire_writer_lock(path.as_ref())?;
        if bootstrap::version(&Connection::open(&path)?)? == 0 {
            replication::restore(&path, environment, replication.storage())?;
        }
        let remote = replication::Remote::load(replication.storage())?;
        let (connection, migrated) = bootstrap::open(&path, environment)?;
        if migrated {
            log::mark_unlogged(&connection)?;
        }
        let epoch = log::epoch(&connection)?;
        let uploaded = remote.uploaded(epoch);
        if remote.latest_epoch().is_some_and(|latest| latest > epoch) || uploaded > log::position(&connection)?.0 {
            return Err(Error::StaleReplica);
        }
        replication::claim(replication.storage(), epoch, &log::claim(&connection)?)?;
        if remote.base(epoch).is_none() {
            // Existing data reaches storage only through a first snapshot.
            log::skip_sequence(&connection)?;
        }
        let (sequence, _) = log::position(&connection)?;
        connection.execute("DELETE FROM _chunk_log WHERE sequence <= ?1", [uploaded])?;
        let (replicator, shared) = Replicator::start(path.clone(), replication, epoch, &remote, sequence)?;
        let store = Self::new(connection, path, writer_lock, Some(log::Log::new(shared, uploaded)))?;
        Ok((store, replicator))
    }

    fn new(
        connection: Connection,
        path: PathBuf,
        writer_lock: bootstrap::WriterLock,
        log: Option<log::Log>,
    ) -> Result<Self> {
        let schema = Arc::new(schema::load(&connection)?);
        let epoch = Epoch(log::epoch(&connection)?);
        Ok(Self { connection, path, schema, epoch, log, _writer_lock: writer_lock })
    }

    /// The environment epoch, fixed for this store's lifetime. Pair it with a
    /// revision to identify a commit across restores.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }
}

fn validate_environment(environment: &str) -> Result<()> {
    if environment.is_empty() || environment.len() > 128 {
        return Err(Error::Invalid("invalid environment identity"));
    }
    Ok(())
}

impl Storage for SqliteStore {
    fn prepare_operation(&mut self, operation: &Operation, context: RetryContext) -> Result<RetryContext> {
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| {
            operations::prepare(transaction, operation, context)
        })
    }

    fn activate_deployment(&mut self, deployment: &chunk_contract::Deployment) -> Result<Revision> {
        let migration = schema::merge(&self.schema, &deployment.tables)?;
        let revision = log::write(&self.connection, self.log.as_mut(), &migration.statements, |transaction| {
            deployments::insert(transaction, deployment)?;
            schema::install(transaction, &migration)
        })?;
        self.schema = Arc::new(migration.schema);
        Ok(revision)
    }

    fn release_deployment(&mut self, id: &str) -> Result<bool> {
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| deployments::release(transaction, id))
    }

    fn deployments(&self) -> Result<Vec<chunk_contract::Deployment>> {
        deployments::load(&self.connection)
    }

    fn retain_deployment(&mut self, deployment: &chunk_contract::Deployment) -> Result<()> {
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| deployments::insert(transaction, deployment))
    }

    fn apply_schema(&mut self, schema: &DatabaseSchema) -> Result<Revision> {
        let migration = schema::merge(&self.schema, schema)?;
        if migration.statements.is_empty() {
            return revision::current(&self.connection);
        }
        let next = log::write(&self.connection, self.log.as_mut(), &migration.statements, |transaction| {
            schema::install(transaction, &migration)
        })?;
        self.schema = Arc::new(migration.schema);
        Ok(next)
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        read::snapshot(&self.path, self.schema.clone())
    }

    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        write::outcome(&self.connection, operation)
    }

    fn jobs(&self) -> Result<crate::Jobs> {
        jobs::load(&self.connection)
    }

    fn job_command(&mut self, command: crate::JobCommand) -> Result<crate::Jobs> {
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| jobs::command(transaction, command))
    }

    fn commit(&mut self, commit: Commit) -> Result<Outcome> {
        self.commit_with_jobs(commit, Vec::new())
    }

    fn commit_with_jobs(&mut self, commit: Commit, intents: Vec<crate::JobIntent>) -> Result<Outcome> {
        if let Some(outcome) = write::outcome(&self.connection, &commit.operation)? {
            return Ok(outcome);
        }
        let prepared = write::Prepared::new(&commit, &self.schema)?;
        let next = log::write(&self.connection, self.log.as_mut(), &[], |transaction| {
            let current = revision::current(transaction)?;
            if current != commit.expected {
                return Err(Error::Conflict { expected: commit.expected, actual: current });
            }
            let next = revision::next(current)?;
            prepared.apply(transaction, next)?;
            jobs::apply(transaction, &intents)?;
            transaction.execute("UPDATE _chunk_metadata SET revision = ?1 WHERE singleton = 1", [next])?;
            transaction.execute(
                "INSERT INTO _chunk_operations VALUES (?1, ?2, ?3, ?4)",
                params![commit.operation.id, commit.operation.fingerprint.as_slice(), next, prepared.result],
            )?;
            transaction.execute("DELETE FROM _chunk_retry_contexts WHERE operation_id = ?1", [&commit.operation.id])?;
            Ok(next)
        })?;
        Ok(Outcome { revision: next, result: commit.result })
    }
}

#[cfg(test)]
mod tests;
