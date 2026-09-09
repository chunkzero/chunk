use chunk_contract::{Field, TableSchema};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use rusqlite::{Connection, OpenFlags, params_from_iter, types::Value as SqlValue};
use serde_json::Value;

use crate::{
    DatabaseSchema, Document, DocumentKey, Error, IndexRange, KeyRange, Operation, Outcome, Result, Snapshot,
    SnapshotReader,
};

use super::{codec, revision, schema};

struct Reader {
    connection: Mutex<Connection>,
    schema: Arc<DatabaseSchema>,
}

pub(super) fn snapshot(path: &Path, schema: Arc<DatabaseSchema>) -> Result<Snapshot> {
    let connection =
        Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.execute_batch("BEGIN")?;
    // The first read establishes the WAL snapshot before any writer can advance it.
    let revision = revision::current(&connection)?;
    Ok(Snapshot::new(
        revision,
        Reader {
            connection: Mutex::new(connection),
            schema,
        },
    ))
}

impl Reader {
    fn table(&self, name: &str) -> Result<&TableSchema> {
        self.schema.get(name).ok_or(Error::Invalid("undeclared table"))
    }

    fn query(&self, table: &TableSchema, sql: &str, params: Vec<SqlValue>) -> Result<Vec<(String, Document)>> {
        let connection = self.connection.lock().map_err(|_| Error::Poisoned)?;
        let mut statement = connection.prepare_cached(sql)?;
        let mut rows = statement.query(params_from_iter(params))?;
        let mut documents = Vec::new();
        while let Some(row) = rows.next()? {
            let id = row.get(0)?;
            let revision = row.get(1)?;
            let mut fields = serde_json::Map::new();
            for (index, (name, field)) in table.fields.iter().enumerate() {
                if let Some(value) = codec::decode(field, row.get(index + 2)?)? {
                    fields.insert(name.clone(), value);
                }
            }
            documents.push((
                id,
                Document {
                    revision,
                    value: Value::Object(fields),
                },
            ));
        }
        Ok(documents)
    }
}

impl SnapshotReader for Reader {
    fn outcome(&self, operation: &Operation) -> Result<Option<Outcome>> {
        let connection = self.connection.lock().map_err(|_| Error::Poisoned)?;
        super::write::outcome(&connection, operation)
    }

    fn schema(&self) -> &DatabaseSchema {
        &self.schema
    }

    fn get(&self, key: &DocumentKey) -> Result<Option<Document>> {
        key.validate()?;
        let table = self.table(&key.table)?;
        let sql = format!(
            "SELECT {} FROM {} WHERE _id = ?",
            select(table),
            codec::quote(&key.table)
        );
        Ok(self
            .query(table, &sql, vec![key.id.clone().into()])?
            .pop()
            .map(|(_, document)| document))
    }

    fn scan(&self, range: &KeyRange) -> Result<Vec<(String, Document)>> {
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
        let sql = format!(
            "SELECT {} FROM {}{predicate} ORDER BY _id",
            select(table),
            codec::quote(&range.table)
        );
        self.query(table, &sql, params)
    }

    fn scan_index(&self, range: &IndexRange) -> Result<Vec<(String, Document)>> {
        let table = self.table(&range.table)?;
        range.validate(table)?;
        if range.end.as_ref().is_some_and(Value::is_null) {
            return Ok(Vec::new());
        }
        let (sql, params) = index_query(table, range)?;
        self.query(table, &sql, params)
    }
}

pub(super) fn index_query(table: &TableSchema, range: &IndexRange) -> Result<(String, Vec<SqlValue>)> {
    range.validate(table)?;
    let fields = &table.indexes[&range.index];
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    for (name, value) in fields.iter().zip(&range.prefix) {
        params.push(index_value(&table.fields[name], value)?);
        conditions.push(format!("{} IS ?", codec::quote(name)));
    }
    if range.start.is_some() || range.end.is_some() {
        let name = fields
            .get(range.prefix.len())
            .ok_or(Error::Invalid("index range has no remaining field"))?;
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
        codec::quote(&range.table),
        codec::quote(&schema::index_name(&range.table, &range.index)),
        order.join(", ")
    );
    params.push(
        i64::try_from(range.limit)
            .map_err(|_| Error::Invalid("index limit"))?
            .into(),
    );
    Ok((sql, params))
}

fn index_value(field: &Field, value: &Value) -> Result<SqlValue> {
    codec::encode(field, (!value.is_null()).then_some(value))
}

fn select(table: &TableSchema) -> String {
    let mut columns = vec!["_id".to_owned(), "_revision".to_owned()];
    columns.extend(table.fields.keys().map(|name| codec::quote(name)));
    columns.join(", ")
}

fn predicate(conditions: &[impl AsRef<str>]) -> String {
    if conditions.is_empty() {
        String::new()
    } else {
        format!(
            " WHERE {}",
            conditions.iter().map(AsRef::as_ref).collect::<Vec<_>>().join(" AND ")
        )
    }
}
