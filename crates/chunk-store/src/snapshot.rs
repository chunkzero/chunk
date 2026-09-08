use std::sync::Arc;

use crate::{DatabaseSchema, Document, DocumentKey, IndexRange, KeyRange, Operation, Outcome, Result, Revision};

/// Adapter-owned reads pinned to one database revision, including schema and indexes.
/// Implementations must keep empty ranges and point misses consistent too.
pub trait SnapshotReader: Send + Sync {
    /// Recovers an outcome visible at this snapshot.
    /// # Errors
    /// Reports mismatched request identities or storage failures.
    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>>;

    /// Schema declarations pinned to the same revision as document reads.
    fn schema(&self) -> &DatabaseSchema;

    /// # Errors
    /// Rejects undeclared tables or invalid keys and reports storage failures.
    fn get(&self, key: &DocumentKey) -> Result<Option<Document>>;

    /// # Errors
    /// Rejects undeclared tables or invalid ranges and reports storage failures.
    fn scan(&self, range: &KeyRange) -> Result<Vec<(String, Document)>>;

    /// # Errors
    /// Rejects undeclared indexes or invalid bounds and reports storage failures.
    fn scan_index(&self, range: &IndexRange) -> Result<Vec<(String, Document)>>;
}

/// Clones share a stable read view; only requested rows are decoded.
/// Drop snapshots promptly so adapters can release retained database history.
#[derive(Clone)]
pub struct Snapshot {
    pub revision: Revision,
    reader: Arc<dyn SnapshotReader>,
}

impl Snapshot {
    /// # Errors
    /// Reports mismatched request identities or storage failures.
    pub fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        self.reader.outcome(operation)
    }

    #[must_use]
    pub fn schema(&self) -> &DatabaseSchema {
        self.reader.schema()
    }

    /// Wraps an adapter's reader already pinned to the supplied revision.
    #[must_use]
    pub fn new(revision: Revision, reader: impl SnapshotReader + 'static) -> Self {
        Self {
            revision,
            reader: Arc::new(reader),
        }
    }

    /// # Errors
    /// Rejects invalid keys or undeclared tables and reports storage failures.
    pub fn get(&self, key: &DocumentKey) -> Result<Option<Document>> {
        self.reader.get(key)
    }

    /// Reads the primary-key interval in ascending document-ID order.
    /// # Errors
    /// Rejects invalid ranges or undeclared tables and reports storage failures.
    pub fn scan(&self, range: &KeyRange) -> Result<Vec<(String, Document)>> {
        self.reader.scan(range)
    }

    /// Reads an index interval, including empty intervals, at this snapshot's revision.
    /// # Errors
    /// Rejects invalid bounds or undeclared indexes and reports storage failures.
    pub fn scan_index(&self, range: &IndexRange) -> Result<Vec<(String, Document)>> {
        self.reader.scan_index(range)
    }
}
