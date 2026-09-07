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
    chunk_contract::validate_name(table).map_err(Error::Invalid)
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

/// An equality prefix followed by an optional half-open range on the next field.
/// Fields follow the declared index order; document ID breaks ties. Null denotes
/// an absent optional scalar and sorts before present values. Complex fields
/// (including nullable unions) cannot be indexed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexRange {
    pub table: String,
    pub index: String,
    pub prefix: Vec<Value>,
    pub start: Option<Value>,
    pub end: Option<Value>,
    /// Maximum rows returned, between 1 and 100,000.
    pub limit: usize,
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
    /// A complete document replacement; None deletes it. Callers implementing
    /// patches must preserve fields they are not changing, including fields
    /// introduced by other retained deployment versions.
    pub value: Option<Value>,
}

pub struct Commit {
    pub expected: Revision,
    pub operation: Operation,
    pub writes: Vec<Write>,
    pub result: Value,
}
