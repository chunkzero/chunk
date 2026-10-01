use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use rusqlite::Connection;

use crate::{
    Commit, DatabaseSchema, Epoch, Error, IndexDefinition, JobIntent, Operation, Outcome, PendingWork, Replication,
    Replicator, Reply, Request, Result, RetryContext, Revision, Snapshot, Storage, Transform, Work, replication,
};

mod backfill;
pub(crate) mod bootstrap;
mod codec;
mod deployments;
mod indexes;
pub(crate) mod jobs;
mod journal;
pub(crate) mod log;
mod operations;
mod read;
mod recount;
pub(crate) mod retention;
mod revision;
mod schema;
mod work;
mod write;

/// Expired outcomes and retry contexts are removed at most this often.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60);

/// Local single-writer adapter. The advisory lock lives beside the canonical
/// database path and is held for this object's lifetime. Every writer must use
/// this boundary; editing the database externally is unsupported.
pub struct SqliteStore {
    connection: Connection,
    schema: Arc<DatabaseSchema>,
    indexes: Arc<BTreeSet<IndexDefinition>>,
    epoch: Epoch,
    log: Option<log::Log>,
    retention: retention::Retention,
    job_limits: jobs::JobLimits,
    /// The backfill work ID and the batch size it has shrunk to, for this process only.
    batch: (u64, usize),
    pruned_at: Option<Instant>,
    readers: Arc<read::Pool>,
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
        let connection = bootstrap::open(&path, environment)?;
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
        Self::replicated(path, writer_lock, environment, replication, &remote)
    }

    /// Creates the database at `path` as `environment` from the latest state
    /// replicated to `source` as `source_environment`, and replicates it to the
    /// empty storage of `replication` under a new lineage starting at epoch 1.
    /// Returns once that storage holds the fork's first snapshot.
    ///
    /// Documents and operation outcomes are copied. Retry contexts are dropped,
    /// and so are pending and running jobs unless `keep_jobs` is set, so a fork
    /// never runs work scheduled by the source.
    /// # Errors
    /// Fails like [`Self::open_replicated`], and with [`Error::Invalid`] when
    /// `path` already holds a database, the target storage is not empty or the
    /// source has no snapshot.
    pub fn fork(
        source: &Replication,
        source_environment: &str,
        path: impl AsRef<Path>,
        environment: &str,
        replication: Replication,
        keep_jobs: bool,
    ) -> Result<(Self, Replicator)> {
        validate_environment(source_environment)?;
        validate_environment(environment)?;
        let (path, writer_lock) = bootstrap::acquire_writer_lock(path.as_ref())?;
        if bootstrap::version(&Connection::open(&path)?)? != 0 {
            return Err(Error::Invalid("fork target database already exists"));
        }
        let target = replication.storage();
        replication::fork(&path, source.storage(), source_environment, environment, target, keep_jobs)?;
        let remote = replication::Remote::load(target)?;
        let (store, replicator) = Self::replicated(path, writer_lock, environment, replication, &remote)?;
        replicator.flush()?;
        Ok((store, replicator))
    }

    fn replicated(
        path: PathBuf,
        writer_lock: bootstrap::WriterLock,
        environment: &str,
        replication: Replication,
        remote: &replication::Remote,
    ) -> Result<(Self, Replicator)> {
        let connection = bootstrap::open(&path, environment)?;
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
        let (replicator, shared) = Replicator::start(path.clone(), replication, epoch, remote, sequence)?;
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
        let indexes = Arc::new(indexes::load(&connection)?);
        let epoch = Epoch(log::epoch(&connection)?);
        let mut store = Self {
            connection,
            readers: read::Pool::new(path),
            schema,
            indexes,
            epoch,
            log,
            retention: retention::Retention::default(),
            job_limits: jobs::JobLimits::default(),
            batch: (0, backfill::BATCH),
            pruned_at: None,
            _writer_lock: writer_lock,
        };
        store.schedule_missing_work()?;
        Ok(store)
    }

    /// Records a build for each index a resident deployment declares that is neither built nor pending, so a
    /// deployment is never unready with nothing to resume, and each drop that can run.
    fn schedule_missing_work(&mut self) -> Result<()> {
        let pending: Vec<_> = work::load(&self.connection)?.into_iter().map(|pending| pending.work).collect();
        let missing: Vec<_> = deployments::load(&self.connection)?
            .iter()
            .flat_map(|deployment| IndexDefinition::declared(&deployment.tables).collect::<Vec<_>>())
            .filter(|index| !self.indexes.contains(index))
            .map(Work::Index)
            .filter(|work| !pending.contains(work))
            .collect();
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| {
            work::record(transaction, missing)?;
            journal::schedule_drops(transaction)
        })
    }

    /// Drops expand migration `expand`'s old shape, unless a resident deployment declares it again.
    fn drop_old_shape(&mut self, id: u64, expand: &str) -> Result<()> {
        let plan = journal::drop_plan(&self.connection, &self.schema, expand)?;
        let statements = plan.as_ref().map_or_else(Vec::new, |(plan, _)| plan.statements.clone());
        log::write_or_roll_back(&self.connection, self.log.as_mut(), &statements, |transaction| {
            if let Some((plan, tables)) = &plan {
                for statement in &statements {
                    transaction.execute_batch(statement)?;
                }
                for table in tables {
                    recount::run(transaction, table, &plan.schema[table])?;
                }
                journal::deactivate(transaction, expand)?;
                schema::replace(transaction, &plan.schema)?;
            }
            work::finish(transaction, id)
        })?;
        if let Some((plan, _)) = plan {
            self.schema = Arc::new(plan.schema);
        }
        Ok(())
    }

    /// Replaces the default one-day retention windows.
    pub fn set_retention(&mut self, retention: retention::Retention) {
        self.retention = retention;
    }

    /// Replaces the default job budget. Existing jobs beyond a lower budget stay.
    pub fn set_job_limits(&mut self, limits: jobs::JobLimits) {
        self.job_limits = limits;
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
        match self.batch(vec![Request::Prepare { operation: operation.clone(), context }]).pop() {
            Some(Ok(Reply::Prepared(context))) => Ok(context),
            Some(Err(error)) => Err(error),
            _ => Err(Error::Corrupt("batch reply")),
        }
    }

    fn install_deployment(&mut self, deployment: &chunk_contract::Deployment) -> Result<Revision> {
        let applied = journal::load(&self.connection)?;
        let (migration, new) = if applied.is_empty() && deployment.contracts.migrations.is_empty() {
            (schema::merge(&self.schema, &deployment.tables, None)?, Vec::new())
        } else {
            journal::install(&self.schema, &applied, deployment)?
        };
        let (current, built) = (&self.schema, &self.indexes);
        let revision =
            log::write_or_roll_back(&self.connection, self.log.as_mut(), &migration.statements, |transaction| {
                let carried = deployments::load(transaction)?
                    .into_iter()
                    .flat_map(|resident| resident.contracts.migrations.into_iter().map(|migration| migration.id))
                    .collect();
                deployments::insert(transaction, deployment)?;
                let missing = IndexDefinition::declared(&deployment.tables).filter(|index| !built.contains(index));
                work::record(transaction, missing.map(Work::Index))?;
                let revision = schema::install(transaction, current, &migration)?;
                journal::record(transaction, applied.len(), &new)?;
                journal::rebackfill(transaction, &applied, deployment, &carried)?;
                journal::schedule_drops(transaction)?;
                Ok(revision)
            })?;
        self.schema = Arc::new(migration.schema);
        Ok(revision)
    }

    fn pending_work(&self) -> Result<Vec<PendingWork>> {
        work::load(&self.connection)
    }

    fn run_work(&mut self, id: u64, transform: &mut Transform<'_>) -> Result<()> {
        let index = match work::get(&self.connection, id)? {
            None => return Ok(()),
            Some((Work::Index(index), ..)) => index,
            Some((Work::Backfill { migration, table }, done, cursor)) => {
                let schema = &self.schema;
                let mut batch = if self.batch.0 == id { self.batch.1 } else { backfill::BATCH };
                let result = log::write_or_roll_back(&self.connection, self.log.as_mut(), &[], |transaction| {
                    let work = (migration.as_str(), table.as_str());
                    backfill::run(transaction, schema, id, work, (done, cursor.as_deref()), &mut batch, transform)
                });
                self.batch = (id, batch);
                return result;
            }
            Some((Work::Drop { migration }, ..)) => return self.drop_old_shape(id, &migration),
        };
        let built = self.indexes.contains(&index);
        let statements = if built { Vec::new() } else { vec![indexes::create(&index)] };
        log::write_or_roll_back(&self.connection, self.log.as_mut(), &statements, |transaction| {
            for statement in &statements {
                transaction.execute_batch(statement)?;
            }
            if !built {
                indexes::record(transaction, std::slice::from_ref(&index))?;
            }
            work::finish(transaction, id)
        })?;
        if !built {
            let mut indexes = (*self.indexes).clone();
            indexes.insert(index);
            self.indexes = Arc::new(indexes);
        }
        Ok(())
    }

    fn release_deployment(&mut self, id: &str) -> Result<bool> {
        let deployments = deployments::load(&self.connection)?;
        let resident = deployments.iter().any(|deployment| deployment.id == id);
        let needed: BTreeSet<_> = deployments
            .iter()
            .filter(|deployment| deployment.id != id)
            .flat_map(|deployment| IndexDefinition::declared(&deployment.tables))
            .chain(IndexDefinition::declared(&self.schema))
            .collect();
        let dropped: Vec<_> = if resident {
            self.indexes.iter().filter(|index| !needed.contains(index)).cloned().collect()
        } else {
            Vec::new()
        };
        let statements: Vec<_> = dropped.iter().map(indexes::drop).collect();
        let removed = log::write(&self.connection, self.log.as_mut(), &statements, |transaction| {
            let removed = deployments::release(transaction, id)?;
            if removed {
                for statement in &statements {
                    transaction.execute_batch(statement)?;
                }
                indexes::forget(transaction, &dropped)?;
                work::prune(transaction, &needed)?;
                journal::schedule_drops(transaction)?;
            }
            Ok(removed)
        })?;
        if !dropped.is_empty() {
            let indexes = self.indexes.iter().filter(|index| !dropped.contains(index)).cloned().collect();
            self.indexes = Arc::new(indexes);
        }
        Ok(removed)
    }

    fn migrations(&self) -> Result<Vec<chunk_contract::Migration>> {
        let applied = journal::load(&self.connection)?;
        Ok(applied.into_iter().filter(|entry| entry.active).map(|entry| entry.migration).collect())
    }

    fn deployments(&self) -> Result<Vec<chunk_contract::Deployment>> {
        deployments::load(&self.connection)
    }

    fn retiring(&self) -> Result<Vec<String>> {
        deployments::retiring(&self.connection)
    }

    fn retain_deployment(&mut self, deployment: &chunk_contract::Deployment) -> Result<()> {
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| deployments::insert(transaction, deployment))
    }

    fn apply_schema(&mut self, schema: &DatabaseSchema) -> Result<Revision> {
        let migration = schema::merge(&self.schema, schema, Some(&self.indexes))?;
        if migration.statements.is_empty() && migration.schema == *self.schema {
            return revision::current(&self.connection);
        }
        let current = &self.schema;
        let next = log::write(&self.connection, self.log.as_mut(), &migration.statements, |transaction| {
            schema::install(transaction, current, &migration)
        })?;
        if !migration.indexes.is_empty() {
            let mut indexes = (*self.indexes).clone();
            indexes.extend(migration.indexes);
            self.indexes = Arc::new(indexes);
        }
        self.schema = Arc::new(migration.schema);
        Ok(next)
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        read::snapshot(&self.readers, self.schema.clone(), self.indexes.clone())
    }

    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        write::outcome(&self.connection, operation)
    }

    fn jobs(&self) -> Result<crate::Jobs> {
        jobs::load(&self.connection)
    }

    fn job_command(&mut self, command: crate::JobCommand) -> Result<crate::Jobs> {
        let (retention, limits) = (&self.retention, &self.job_limits);
        log::write(&self.connection, self.log.as_mut(), &[], |transaction| {
            // Pruning changes the wake generation, which would make an acknowledgement stale.
            let prune = !matches!(command, crate::JobCommand::AcknowledgeWake { .. });
            let jobs = jobs::command(transaction, command, limits)?;
            if prune && retention::prune_jobs(transaction, retention, retention::now())? {
                jobs::changed(transaction)?;
                return jobs::load(transaction);
            }
            Ok(jobs)
        })
    }

    fn commit(&mut self, commit: Commit) -> Result<Outcome> {
        self.commit_with_jobs(commit, Vec::new())
    }

    fn commit_with_jobs(&mut self, commit: Commit, intents: Vec<JobIntent>) -> Result<Outcome> {
        match self.batch(vec![Request::Commit { commit, intents }]).pop() {
            Some(Ok(Reply::Committed(outcome))) => Ok(outcome),
            Some(Err(error)) => Err(error),
            _ => Err(Error::Corrupt("batch reply")),
        }
    }

    /// Shares one transaction and fsync. Each request runs in a savepoint, so a
    /// rejection undoes only that request; any other failure undoes the batch.
    fn batch(&mut self, requests: Vec<Request>) -> Vec<Result<Reply>> {
        let now = retention::now();
        let prune = self.pruned_at.is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL);
        let (schema, retention, limits) = (&self.schema, &self.retention, &self.job_limits);
        let mut backlog = false;
        let committed = log::write(&self.connection, self.log.as_mut(), &[], |transaction| {
            if prune {
                backlog = retention::prune_operations(transaction, retention, now)?;
            }
            let mut results = Vec::with_capacity(requests.len());
            for request in requests {
                transaction.execute_batch("SAVEPOINT chunk_commit")?;
                let result = match request {
                    Request::Prepare { operation, context } => {
                        operations::prepare(transaction, &operation, context).map(Reply::Prepared)
                    }
                    Request::Commit { commit, intents } => {
                        // Expired finished jobs must not hold capacity a new job needs.
                        let scheduling = intents.iter().any(|intent| matches!(intent, JobIntent::Schedule(_)));
                        (if scheduling { retention::prune_jobs(transaction, retention, now) } else { Ok(false) })
                            .and_then(|_| write::commit(transaction, schema, commit, &intents, limits, now))
                            .map(Reply::Committed)
                    }
                };
                match &result {
                    Ok(_) => transaction.execute_batch("RELEASE chunk_commit")?,
                    Err(error) if error.rejected() => {
                        transaction.execute_batch("ROLLBACK TO chunk_commit; RELEASE chunk_commit")?;
                    }
                    Err(_) => return result.map(|_| Vec::new()),
                }
                results.push(result);
            }
            Ok(results)
        });
        match committed {
            Ok(results) => {
                // Each pass is bounded; a remaining backlog keeps the next write pruning.
                if prune {
                    self.pruned_at = (!backlog).then(Instant::now);
                }
                results
            }
            Err(error) => vec![Err(error)],
        }
    }

    fn epoch(&self) -> Epoch {
        self.epoch
    }
}

#[cfg(test)]
mod tests;
