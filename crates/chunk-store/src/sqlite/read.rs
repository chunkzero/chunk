use chunk_contract::{Field, TableSchema};
use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use crate::{
    DatabaseSchema, Document, DocumentKey, Error, IndexDefinition, IndexRange, KeyRange, Operation, Outcome,
    ReadBudget, Result, Revision, Snapshot, SnapshotReader,
};

use super::{codec, indexes, revision};

/// Idle read connections beyond this are closed instead of kept.
const IDLE: usize = 8;

/// Read-only connections reused across snapshots, so their prepared statements
/// stay compiled. A connection is idle only outside a transaction.
pub(super) struct Pool {
    path: PathBuf,
    idle: Mutex<Vec<Connection>>,
}

impl Pool {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self { path, idle: Mutex::new(Vec::new()) })
    }

    #[cfg(test)]
    pub fn idle(&self) -> usize {
        self.idle.lock().unwrap().len()
    }

    fn lease(self: &Arc<Self>) -> Result<Lease> {
        let idle = self.idle.lock().map_err(|_| Error::Poisoned)?.pop();
        let connection = if let Some(connection) = idle {
            connection
        } else {
            let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
            let connection = Connection::open_with_flags(&self.path, flags)?;
            connection.busy_timeout(Duration::from_secs(5))?;
            connection.set_prepared_statement_cache_capacity(64);
            connection
        };
        Ok(Lease { connection: Some(connection), pool: self.clone() })
    }
}

/// A pooled connection that ends its read transaction and returns on drop.
struct Lease {
    connection: Option<Connection>,
    pool: Arc<Pool>,
}

impl Lease {
    fn connection(&self) -> &Connection {
        self.connection.as_ref().expect("leased until dropped")
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let Some(connection) = self.connection.take() else { return };
        if !connection.is_autocommit() && connection.execute_batch("ROLLBACK").is_err() {
            return;
        }
        if let Ok(mut idle) = self.pool.idle.lock()
            && idle.len() < IDLE
        {
            idle.push(connection);
        }
    }
}

struct Reader {
    connection: Mutex<Lease>,
    schema: Arc<DatabaseSchema>,
    indexes: Arc<BTreeSet<IndexDefinition>>,
}

pub(super) fn snapshot(
    pool: &Arc<Pool>,
    schema: Arc<DatabaseSchema>,
    indexes: Arc<BTreeSet<IndexDefinition>>,
) -> Result<Snapshot> {
    let lease = pool.lease()?;
    lease.connection().execute_batch("BEGIN")?;
    // The first read establishes the WAL snapshot before any writer can advance it.
    let revision = revision::current(lease.connection())?;
    Ok(Snapshot::new(revision, Reader { connection: Mutex::new(lease), schema, indexes }))
}

/// A row copied out of SQLite, decoded once the connection is released.
struct Raw {
    id: String,
    revision: Revision,
    fields: Vec<SqlValue>,
}

impl Reader {
    fn table(&self, name: &str) -> Result<&TableSchema> {
        self.schema.get(name).ok_or(Error::Invalid("undeclared table"))
    }

    fn query(
        &self,
        table: &TableSchema,
        sql: &str,
        params: Vec<SqlValue>,
        budget: &mut ReadBudget,
    ) -> Result<Vec<(String, Document)>> {
        let rows = self.rows(table, sql, params, budget)?;
        rows.into_iter()
            .map(|raw| {
                let mut fields = serde_json::Map::new();
                for ((name, field), value) in table.fields.iter().zip(raw.fields) {
                    if let Some(value) = codec::decode(field, value)? {
                        fields.insert(name.clone(), value);
                    }
                }
                Ok((raw.id, Document { revision: raw.revision, value: Value::Object(fields) }))
            })
            .collect()
    }

    /// Steps and copies rows while holding the connection; decoding happens after.
    fn rows(&self, table: &TableSchema, sql: &str, params: Vec<SqlValue>, budget: &mut ReadBudget) -> Result<Vec<Raw>> {
        let lease = self.connection.lock().map_err(|_| Error::Poisoned)?;
        let mut statement = lease.connection().prepare_cached(sql)?;
        let mut rows = statement.query(params_from_iter(params))?;
        let mut copied = Vec::new();
        while let Some(row) = rows.next()? {
            let mut bytes = row.get::<_, usize>(2)?;
            let raw_bytes = (3..table.fields.len() + 3).try_fold(0, |total, index| -> Result<usize> {
                let value = row.get_ref(index)?;
                let length = match value {
                    rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => bytes.len(),
                    _ => 8,
                };
                Ok(total + length)
            })?;
            bytes = bytes.max(raw_bytes)
                + row.get_ref(0)?.as_bytes().map_err(|_| Error::Corrupt("invalid document ID"))?.len();
            budget.charge(bytes)?;
            let fields = (3..table.fields.len() + 3).map(|index| row.get(index)).collect::<rusqlite::Result<_>>()?;
            copied.push(Raw { id: row.get(0)?, revision: row.get(1)?, fields });
        }
        Ok(copied)
    }
}

impl SnapshotReader for Reader {
    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        let lease = self.connection.lock().map_err(|_| Error::Poisoned)?;
        super::write::outcome(lease.connection(), operation)
    }

    fn schema(&self) -> &DatabaseSchema {
        &self.schema
    }

    fn indexes(&self) -> &BTreeSet<IndexDefinition> {
        &self.indexes
    }

    fn get(&self, key: &DocumentKey, budget: &mut ReadBudget) -> Result<Option<Document>> {
        key.validate()?;
        let table = self.table(&key.table)?;
        let sql = format!("SELECT {} FROM {} WHERE _id = ?", select(table), codec::quote(&key.table));
        Ok(self.query(table, &sql, vec![key.id.clone().into()], budget)?.pop().map(|(_, document)| document))
    }

    fn scan(&self, range: &KeyRange, budget: &mut ReadBudget) -> Result<Vec<(String, Document)>> {
        range.validate()?;
        let table = self.table(&range.table)?;
        let mut conditions = Vec::new();
        let mut params = Vec::new();
        if let Some(start) = &range.start {
            conditions.push("_id >= ?");
            params.push(start.clone().into());
        }
        if let Some(end) = &range.end {
            conditions.push("_id < ?");
            params.push(end.clone().into());
        }
        let predicate = predicate(&conditions);
        let sql = format!("SELECT {} FROM {}{predicate} ORDER BY _id", select(table), codec::quote(&range.table));
        self.query(table, &sql, params, budget)
    }

    fn scan_index(&self, range: &IndexRange, budget: &mut ReadBudget) -> Result<Vec<(String, Document)>> {
        let table = self.table(&range.index.table)?;
        range.validate(table)?;
        if !self.indexes.contains(&range.index) {
            return Err(Error::Invalid("index is not built"));
        }
        if range.end.as_ref().is_some_and(Value::is_null) {
            return Ok(Vec::new());
        }
        let (sql, params) = index_query(table, range)?;
        self.query(table, &sql, params, budget)
    }
}

/// Builds the SQL for an already validated range; tests inspect its query plan.
pub(super) fn index_query(table: &TableSchema, range: &IndexRange) -> Result<(String, Vec<SqlValue>)> {
    let fields = &range.index.fields;
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    for (name, value) in fields.iter().zip(&range.prefix) {
        params.push(index_value(&table.fields[name], value)?);
        conditions.push(format!("{} IS ?", codec::quote(name)));
    }
    if range.start.is_some() || range.end.is_some() {
        let name = fields.get(range.prefix.len()).ok_or(Error::Invalid("index range has no remaining field"))?;
        let field = &table.fields[name];
        let column = codec::quote(name);
        if let Some(start) = &range.start {
            let value = index_value(field, start)?;
            if value != SqlValue::Null {
                conditions.push(format!("{column} >= ?"));
                params.push(value);
            }
        }
        if let Some(end) = &range.end {
            let value = index_value(field, end)?;
            if value != SqlValue::Null {
                conditions.push(if field.optional {
                    format!("({column} IS NULL OR {column} < ?)")
                } else {
                    format!("{column} < ?")
                });
                params.push(value);
            }
        }
    }
    let predicate = predicate(&conditions);
    let mut order: Vec<_> = fields.iter().map(|name| codec::quote(name)).collect();
    order.push("_id".into());
    let sql = format!(
        "SELECT {} FROM {} INDEXED BY {}{predicate} ORDER BY {} LIMIT ?",
        select(table),
        codec::quote(&range.index.table),
        codec::quote(&indexes::name(&range.index)),
        order.join(", ")
    );
    params.push(i64::try_from(range.limit).map_err(|_| Error::Invalid("index limit"))?.into());
    Ok((sql, params))
}

fn index_value(field: &Field, value: &Value) -> Result<SqlValue> {
    codec::encode(field, (!value.is_null()).then_some(value))
}

fn select(table: &TableSchema) -> String {
    let mut columns = vec!["_id".to_owned(), "_revision".to_owned(), "_bytes".to_owned()];
    columns.extend(table.fields.keys().map(|name| codec::quote(name)));
    columns.join(", ")
}

fn predicate(conditions: &[impl AsRef<str>]) -> String {
    if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" AND "))
    }
}
