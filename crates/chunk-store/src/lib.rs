//! Environment persistence, independent of JavaScript and subscriptions.
//!
//! A snapshot is immutable and internally consistent. The engine validates its
//! dependencies against the latest snapshot, then commits against that revision.
//! SQLite serializes the final compare-and-commit and durably records the result
//! in the same transaction as document changes. Retry an unknown outcome using
//! the same operation identity, never a newly generated ID.

mod model;
mod sqlite;

pub use model::{Commit, Document, DocumentKey, KeyRange, Operation, Outcome, Revision, Snapshot, Write};
pub use sqlite::SqliteStore;

/// Only the database's single owning service holds this capability.
pub trait Storage: Send {
    /// # Errors
    /// Returns I/O, corruption or snapshot-limit errors.
    fn snapshot(&mut self) -> Result<Snapshot>;

    /// Recovers an operation's durable result, including after backend restart.
    /// # Errors
    /// Rejects reuse of an ID for a different request and reports storage failures.
    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>>;

    /// Atomically applies changes and records their outcome, or changes nothing.
    /// An already committed operation returns its original outcome before checking
    /// the expected revision. Other stale revisions return `Conflict`.
    /// # Errors
    /// Returns conflicts, invalid batches, mismatched operations or storage failures.
    fn commit(&mut self, commit: Commit) -> Result<Outcome>;
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
    #[error("local database size limit reached")]
    Capacity,
    #[error("storage I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("JSON: {0}")]
    Json(#[from] serde_json::Error),
}
