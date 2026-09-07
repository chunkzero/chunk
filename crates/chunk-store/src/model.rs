use std::{collections::BTreeMap, ops::Bound};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// Monotonic environment commit order, including commits with no document writes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Revision(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DocumentKey {
    pub table: String,
    pub id: String,
}

impl DocumentKey {
    /// # Errors
    /// Rejects invalid table names and empty or oversized IDs.
    pub fn new(table: impl Into<String>, id: impl Into<String>) -> Result<Self> {
        let key = Self {
            table: table.into(),
            id: id.into(),
        };
        key.validate()?;
        Ok(key)
    }

    pub(crate) fn validate(&self) -> Result<()> {
        validate_table(&self.table)?;
        validate_id(&self.id)
    }
}

fn validate_table(table: &str) -> Result<()> {
    if table.is_empty() || table.len() > 64 || !table.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return Err(Error::Invalid("invalid table name"));
    }
    Ok(())
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.contains('\0') {
        return Err(Error::Invalid("invalid document ID"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub revision: Revision,
    pub value: Value,
}

/// Primary-key index interval: inclusive start, exclusive end; None is unbounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyRange {
    pub table: String,
    pub start: Option<String>,
    pub end: Option<String>,
}

impl KeyRange {
    /// # Errors
    /// Rejects reversed bounds and invalid table names.
    pub fn validate(&self) -> Result<()> {
        validate_table(&self.table)?;
        if self
            .start
            .as_ref()
            .zip(self.end.as_ref())
            .is_some_and(|(start, end)| start > end)
        {
            return Err(Error::Invalid("reversed index range"));
        }
        Ok(())
    }
}

/// Materialized snapshots keep execution independent of SQLite transaction lifetimes.
/// The local adapter caps total documents/bytes; old snapshots remain valid until dropped.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub revision: Revision,
    tables: BTreeMap<String, BTreeMap<String, Document>>,
}

impl Snapshot {
    /// Creates a snapshot from an adapter's complete table map at the given revision.
    /// The adapter is responsible for supplying internally consistent documents.
    ///
    /// ```
    /// use chunk_store::{Document, DocumentKey, Revision, Snapshot};
    /// use serde_json::json;
    ///
    /// let tables = [("profiles".into(), [("a".into(), Document {
    ///     revision: Revision(1),
    ///     value: json!({"coins": 4}),
    /// })].into())].into();
    /// let snapshot = Snapshot::new(Revision(1), tables);
    /// assert_eq!(snapshot.get(&DocumentKey::new("profiles", "a")?).unwrap().value,
    ///     json!({"coins": 4}));
    /// # Ok::<(), chunk_store::Error>(())
    /// ```
    #[must_use]
    pub fn new(revision: Revision, tables: BTreeMap<String, BTreeMap<String, Document>>) -> Self {
        Self { revision, tables }
    }

    #[must_use]
    pub fn get(&self, key: &DocumentKey) -> Option<&Document> {
        self.tables.get(&key.table)?.get(&key.id)
    }

    /// Reads the ordered primary-key index, including empty intervals.
    /// # Errors
    /// Rejects invalid ranges.
    pub fn scan<'a>(&'a self, range: &KeyRange) -> Result<Vec<(&'a str, &'a Document)>> {
        range.validate()?;
        let Some(table) = self.tables.get(&range.table) else {
            return Ok(Vec::new());
        };
        let start = range.start.as_deref().map_or(Bound::Unbounded, Bound::Included);
        let end = range.end.as_deref().map_or(Bound::Unbounded, Bound::Excluded);
        Ok(table
            .range::<str, _>((start, end))
            .map(|(id, doc)| (id.as_str(), doc))
            .collect())
    }
}

/// The backend hashes deployment, contract, function, caller and validated arguments.
/// Stable identity lets a lost acknowledgement be recovered without reexecution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    pub id: String,
    pub fingerprint: [u8; 32],
}

impl Operation {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.id.is_empty() || self.id.len() > 256 {
            return Err(Error::Invalid("invalid operation ID"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Outcome {
    pub revision: Revision,
    pub result: Value,
}

#[derive(Debug, Clone)]
pub struct Write {
    pub key: DocumentKey,
    /// None deletes the document.
    pub value: Option<Value>,
}

pub struct Commit {
    pub expected: Revision,
    pub operation: Operation,
    pub writes: Vec<Write>,
    pub result: Value,
}
