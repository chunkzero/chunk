use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Error, Result};

/// Cumulative decoded-read allowance. Adapters charge before allocating documents.
pub struct ReadBudget {
    rows: usize,
    bytes: usize,
}

impl ReadBudget {
    #[must_use]
    pub fn new(rows: usize, bytes: usize) -> Self {
        Self { rows, bytes }
    }

    /// # Errors
    /// Rejects a row before decoding when either allowance would be exceeded.
    pub fn charge(&mut self, bytes: usize) -> Result<()> {
        if self.rows == 0 || bytes > self.bytes {
            return Err(Error::ReadLimit);
        }
        self.rows -= 1;
        self.bytes -= bytes;
        Ok(())
    }
}

impl Default for ReadBudget {
    fn default() -> Self {
        Self::new(100_000, 32 * 1024 * 1024)
    }
}

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
        let key = Self { table: table.into(), id: id.into() };
        key.validate()?;
        Ok(key)
    }

    /// # Errors
    /// Rejects invalid table names and empty or oversized IDs.
    pub fn validate(&self) -> Result<()> {
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
/// Returns every matching document, bounded by the adapter environment capacity
/// (100,000 documents / 32 MiB for SQLite).
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
        if self.start.as_ref().zip(self.end.as_ref()).is_some_and(|(start, end)| start > end) {
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

impl IndexRange {
    /// Validates against a complete table declaration.
    /// # Errors
    /// Rejects invalid names, limits, prefix lengths, bound types and reversed bounds.
    pub fn validate(&self, table: &chunk_contract::TableSchema) -> Result<()> {
        validate_table(&self.table)?;
        chunk_contract::validate_name(&self.index).map_err(Error::Invalid)?;
        let fields = table.indexes.get(&self.index).ok_or(Error::Invalid("undeclared index"))?;
        if self.limit == 0 || self.limit > 100_000 || self.prefix.len() > fields.len() {
            return Err(Error::Invalid("invalid index range"));
        }
        let validate_value = |name: &str, value: &Value| -> Result<()> {
            let field = table.fields.get(name).ok_or(Error::Invalid("undeclared index field"))?;
            if !field.schema.is_scalar()
                || !(if value.is_null() { field.optional } else { field.schema.accepts(value) })
            {
                return Err(Error::Invalid("invalid index value"));
            }
            Ok(())
        };
        for (name, value) in fields.iter().zip(&self.prefix) {
            validate_value(name, value)?;
        }
        if self.start.is_some() || self.end.is_some() {
            let name = fields.get(self.prefix.len()).ok_or(Error::Invalid("index range has no remaining field"))?;
            for value in self.start.iter().chain(self.end.iter()) {
                validate_value(name, value)?;
            }
        }
        if self
            .start
            .as_ref()
            .zip(self.end.as_ref())
            .is_some_and(|(start, end)| chunk_contract::compare_index_values(start, end).is_gt())
        {
            return Err(Error::Invalid("reversed index range"));
        }
        Ok(())
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

/// Durable invocation inputs fixed before the first evaluation of an operation.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RetryContext {
    pub deployment: String,
    pub timestamp: i64,
    pub seed: u64,
}
