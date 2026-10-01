//! Environment persistence, independent of JavaScript and subscriptions.
//!
//! A snapshot is immutable and internally consistent. The engine validates its
//! dependencies against the latest snapshot, then commits against that revision.
//! SQLite serializes the final compare-and-commit and durably records the result
//! in the same transaction as document changes. Retry an unknown outcome using
//! the same operation identity, never a newly generated ID.
//!
//! Declare physical tables before writing. Scalar fields become native SQL
//! columns; objects, arrays, literals and unions use JSON text. SQL NULL encodes
//! absence, while JSON `null` remains a present value. Indexes cover declared
//! scalar fields with document ID as the final ordering tiebreaker. An index is
//! identified by its table, name and fields together.
//!
//! ```
//! use chunk_store::{Commit, DatabaseSchema, DocumentKey, IndexDefinition, IndexRange,
//!     Operation, SqliteStore, Storage, Write};
//! use serde_json::json;
//!
//! # let directory = tempfile::tempdir()?;
//! let mut store = SqliteStore::open(directory.path().join("data.db"), "local")?;
//! let schema: DatabaseSchema = serde_json::from_value(json!({
//!     "profiles": {
//!         "fields": {
//!             "player": {"schema": {"type": "string"}},
//!             "wins": {"schema": {"type": "integer"}}
//!         },
//!         "indexes": {"by_player": ["player"]}
//!     }
//! }))?;
//! let revision = store.apply_schema(&schema)?;
//! store.commit(Commit {
//!     expected: revision,
//!     // The backend supplies a stable operation ID and request fingerprint.
//!     operation: Operation { id: "create-profile".into(), fingerprint: [7; 32] },
//!     writes: vec![Write {
//!         key: DocumentKey::new("profiles", "profile-1")?,
//!         value: Some(json!({"player": "alex", "wins": 0})),
//!     }],
//!     result: json!("profile-1"),
//! })?;
//! let snapshot = store.snapshot()?;
//! let index = IndexDefinition {
//!     table: "profiles".into(), name: "by_player".into(), fields: vec!["player".into()],
//! };
//! let rows = snapshot.scan_index(&IndexRange {
//!     index, prefix: vec![json!("alex")], start: None, end: None, limit: 1,
//! })?;
//! assert_eq!(rows[0].0, "profile-1");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod jobs;
mod model;
mod replication;
mod snapshot;
mod sqlite;
mod work;

/// The largest stored document, as JSON.
pub const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;

pub use chunk_contract::{DatabaseSchema, SYSTEM_TABLE_PREFIX, is_system_table};
pub use jobs::{Job, JobCommand, JobIntent, JobState, Jobs, WakeHandoff};
pub use model::{
    Commit, Document, DocumentKey, Epoch, IndexDefinition, IndexRange, KeyRange, Operation, Outcome, ReadBudget,
    RetryContext, Revision, Write,
};
pub use replication::{Listed, ObjectStorage, Replication, ReplicationProgress, Replicator, S3Bucket, S3Credentials};
pub use snapshot::{Snapshot, SnapshotReader};
pub use sqlite::{SqliteStore, jobs::JobLimits, retention::Retention};
pub use work::{PendingWork, Transform, Work, compatible_field};

/// Only the database's single owning service holds this capability.
pub trait Storage: Send {
    /// Durably fixes invocation time, seed and deployment before evaluation.
    /// A retry within the retry-context retention window gets the stored context.
    /// # Errors
    /// Rejects reused identities, changed deployment bindings or storage failures.
    fn prepare_operation(&mut self, operation: &Operation, context: RetryContext) -> Result<RetryContext>;

    /// Retains a deployment, applies its schema and records the work it waits
    /// on, such as building its indexes, in one transaction. Installing it
    /// again records work it still lacks.
    ///
    /// Without a migration journal on either side, its new tables and optional
    /// fields are installed. Otherwise its journal and the environment's applied
    /// migrations must be prefix-related with matching hashes, and the stored
    /// schema must match the applied ones. The journal's new entries are
    /// applied: an expand adds its fields and records their backfills, and a
    /// finish schedules dropping the old shape once no resident deployment
    /// declares it. A shorter journal installs only while the store holds
    /// every field the deployment declares.
    /// # Errors
    /// Rejects incompatible schemas, [system tables](is_system_table), retired
    /// identities and storage failures, and journals with
    /// [`Error::Migration`]. A storage failure that changed nothing is
    /// [`Error::RolledBack`].
    fn install_deployment(&mut self, deployment: &chunk_contract::Deployment) -> Result<Revision>;

    /// Work installed deployments wait on, in the order it runs.
    /// # Errors
    /// Reports storage failures or corrupt work records.
    fn pending_work(&self) -> Result<Vec<PendingWork>>;

    /// Runs one item of pending work, or one batch of a backfill through
    /// `transform`, in its own write, without changing document revisions. Work
    /// no longer pending is done already.
    /// # Errors
    /// Reports storage failures, and transforms that fail or produce invalid
    /// rows with [`Error::Migration`]. A failure that changed nothing is
    /// [`Error::RolledBack`].
    fn run_work(&mut self, id: u64, transform: &mut Transform<'_>) -> Result<()>;

    /// Applied expand migrations whose old shape is still stored, in journal order.
    /// # Errors
    /// Reports storage failures or corrupt records.
    fn migrations(&self) -> Result<Vec<chunk_contract::Migration>>;

    /// Removes an inactive deployment, permanently retiring its identity. Indexes
    /// and pending work that no remaining deployment or schema declares go with it.
    /// # Errors
    /// Reports storage failures; the caller must first drain references.
    fn release_deployment(&mut self, id: &str) -> Result<bool>;

    /// Loads retained immutable deployment metadata and bundles.
    /// # Errors
    /// Reports I/O, corruption or unsupported metadata.
    fn deployments(&self) -> Result<Vec<chunk_contract::Deployment>>;

    /// The retained deployments a cancellation of their jobs recorded as retiring, which accept no more work.
    /// # Errors
    /// Reports I/O or corruption.
    fn retiring(&self) -> Result<Vec<String>>;

    /// Durably retains an immutable deployment without changing document revisions.
    /// The backend must establish schema readiness before exposing its functions.
    /// # Errors
    /// Rejects changed identities, invalid declarations and excessive retention.
    fn retain_deployment(&mut self, deployment: &chunk_contract::Deployment) -> Result<()>;

    /// Atomically installs new tables, optional fields and indexes, advancing the
    /// environment revision. Reapplying declarations is a no-op. Omitted tables,
    /// fields and indexes are retained; existing definitions cannot be changed.
    /// Unlike a deployment's, these indexes are built in the same transaction and
    /// are never dropped.
    /// # Errors
    /// Rejects incompatible or invalid schemas and reports storage failures.
    fn apply_schema(&mut self, schema: &DatabaseSchema) -> Result<Revision>;

    /// # Errors
    /// Returns I/O or corruption errors.
    fn snapshot(&mut self) -> Result<Snapshot>;

    /// Recovers an operation's durable result, including after backend restart,
    /// until its outcome retention expires.
    /// # Errors
    /// Rejects reuse of an ID for a different request and reports storage failures.
    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>>;

    /// Loads bounded durable scheduling state.
    /// # Errors
    /// Reports storage failures or corrupt job metadata.
    fn jobs(&self) -> Result<Jobs> {
        Ok(Jobs::default())
    }

    /// Changes host-owned scheduling state without changing document revisions.
    /// # Errors
    /// Rejects invalid transitions, stale alarm acknowledgements or unsupported scheduling.
    fn job_command(&mut self, command: JobCommand) -> Result<Jobs> {
        if matches!(command, JobCommand::Recover) {
            return self.jobs();
        }
        Err(Error::Invalid("durable scheduling unsupported"))
    }

    /// Commits scheduling intents with document writes and operation outcome.
    /// # Errors
    /// Nonempty intents fail closed unless the adapter implements atomic scheduling.
    fn commit_with_jobs(&mut self, commit: Commit, intents: Vec<JobIntent>) -> Result<Outcome> {
        if !intents.is_empty() {
            return Err(Error::Invalid("durable scheduling unsupported"));
        }
        self.commit(commit)
    }

    /// Atomically applies changes and records their outcome, or changes nothing.
    /// An already committed operation returns its original outcome before checking
    /// the expected revision, while that outcome is retained; afterwards it commits
    /// again as a new operation. Other stale revisions return `Conflict`.
    /// # Errors
    /// Returns conflicts, invalid batches, mismatched operations or storage failures.
    fn commit(&mut self, commit: Commit) -> Result<Outcome>;

    /// Applies requests in order as if one at a time, each commit with its own
    /// revision and outcome, returning one result per request until the first
    /// failure that is not a [rejection](Error::rejected). Requests after it have
    /// no result and were not applied. Adapters may share one durable write.
    fn batch(&mut self, requests: Vec<Request>) -> Vec<Result<Reply>> {
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            let result = match request {
                Request::Prepare { operation, context } => {
                    self.prepare_operation(&operation, context).map(Reply::Prepared)
                }
                Request::Commit { commit, intents } => self.commit_with_jobs(commit, intents).map(Reply::Committed),
            };
            let stop = result.as_ref().is_err_and(|error| !error.rejected());
            results.push(result);
            if stop {
                break;
            }
        }
        results
    }

    /// Advances on every restore, so `(epoch, revision)` identifies a commit across restores.
    fn epoch(&self) -> Epoch;
}

/// A durable write that may share a transaction and fsync with others.
pub enum Request {
    Prepare { operation: Operation, context: RetryContext },
    Commit { commit: Commit, intents: Vec<JobIntent> },
}

#[derive(Debug)]
pub enum Reply {
    Prepared(RetryContext),
    Committed(Outcome),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("another backend holds the environment writer lock")]
    WriterLocked,
    #[error("database belongs to another environment")]
    EnvironmentMismatch,
    #[error("unsupported storage schema version {0}")]
    SchemaVersion(i64),
    #[error("revision conflict: expected {expected:?}, found {actual:?}")]
    Conflict { expected: Revision, actual: Revision },
    #[error("operation ID was already used for a different request")]
    OperationMismatch,
    #[error("invalid persistence request: {0}")]
    Invalid(&'static str),
    #[error("{0}")]
    Migration(String),
    #[error("local database size limit reached")]
    Capacity,
    #[error("scheduled job budget reached; retry once jobs finish or expire")]
    JobBudget,
    #[error("snapshot read budget exceeded")]
    ReadLimit,
    #[error("corrupt storage: {0}")]
    Corrupt(&'static str),
    #[error("snapshot connection was poisoned")]
    Poisoned,
    #[error("object storage holds a newer history of this environment than the local database")]
    StaleReplica,
    #[error("replication: {0}")]
    Replication(String),
    #[error("another store claimed a newer epoch of this environment; stop serving it")]
    Fenced,
    #[error("write rolled back: {0}")]
    RolledBack(Box<Error>),
    #[error("storage I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// A rejected request changed nothing and leaves storage usable.
    #[must_use]
    pub fn rejected(&self) -> bool {
        matches!(
            self,
            Self::Conflict { .. }
                | Self::Invalid(_)
                | Self::Migration(_)
                | Self::Capacity
                | Self::JobBudget
                | Self::OperationMismatch
                | Self::RolledBack(_)
        )
    }
}

#[cfg(test)]
mod tests;
