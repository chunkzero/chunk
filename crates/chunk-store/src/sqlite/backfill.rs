//! Backfills apply an expand migration's `to` to a table in batches ordered by ID. A batch is read, transformed
//! outside the store, and committed in a write that updates only the rows that haven't changed since they were
//! read: a document written before the backfill reaches it was synchronized by its write.

use rusqlite::{Connection, params, params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value};

use crate::{Backfill, BackfillRow, DatabaseSchema, Error, MAX_DOCUMENT_BYTES, Result};

use super::{
    codec::{self, quote},
    journal, work,
    write::MAX_DOCUMENT_TOTAL_BYTES,
};

/// The rows of a full batch.
pub(crate) const BATCH: usize = 256;
/// Bounds the documents one batch holds in memory.
const BATCH_BYTES: usize = 512 * 1024;

/// Reads `fields` of a row's columns, after `_id`, `_revision` and `_bytes`.
fn decode<'a>(
    row: &rusqlite::Row<'_>,
    fields: impl Iterator<Item = (&'a String, &'a chunk_contract::Field)>,
) -> Result<Map<String, Value>> {
    let mut decoded = Map::new();
    for (index, (name, field)) in fields.enumerate() {
        if let Some(value) = codec::decode(field, row.get::<_, SqlValue>(index + 3)?)? {
            decoded.insert(name.clone(), value);
        }
    }
    Ok(decoded)
}

fn select<'a>(table: &str, fields: impl Iterator<Item = &'a String>, filter: &str) -> String {
    let mut columns = vec!["_id".to_owned(), "_revision".to_owned(), "_bytes".to_owned()];
    columns.extend(fields.map(|field| quote(field)));
    format!("SELECT {} FROM {} {filter}", columns.join(", "), quote(table))
}

/// Reads the next batch of backfill `id`, or finishes it when no rows remain.
pub(super) fn read(
    connection: &Connection,
    schema: &DatabaseSchema,
    id: u64,
    (migration, table): (&str, &str),
    (cursor, limit): (Option<&str>, usize),
) -> Result<Option<Backfill>> {
    let applied = journal::load(connection)?;
    let expand = applied.iter().find(|entry| entry.migration.id == migration).ok_or(Error::Corrupt("backfill"))?;
    let stored = schema.get(table).ok_or(Error::Corrupt("backfill"))?;
    let readable = expand.migration.input_fields(table, false);
    let inputs = stored.fields.iter().filter(|(name, _)| readable.contains(name.as_str()));
    let sql = select(table, inputs.clone().map(|(name, _)| name), "WHERE _id > ?1 ORDER BY _id LIMIT ?2");
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut read = 0;
    let mut statement = connection.prepare(&sql)?;
    let mut query = statement.query(params![cursor.unwrap_or(""), limit])?;
    while let Some(row) = query.next()? {
        read += 1;
        let (row_id, revision, row_bytes): (String, u64, usize) = (row.get(0)?, row.get(1)?, row.get(2)?);
        if !rows.is_empty() && bytes + row_bytes > BATCH_BYTES {
            break;
        }
        bytes += row_bytes;
        let mut input = decode(row, inputs.clone())?;
        input.insert("_id".into(), Value::String(row_id.clone()));
        rows.push(BackfillRow { id: row_id, revision, input: Value::Object(input) });
    }
    drop(query);
    drop(statement);
    if rows.is_empty() {
        work::finish(connection, id)?;
        journal::schedule_drops(connection)?;
        return Ok(None);
    }
    let complete = read < limit && rows.len() == read;
    Ok(Some(Backfill {
        migration: migration.into(),
        table: table.into(),
        cursor: cursor.map(Into::into),
        rows,
        complete,
    }))
}

/// Writes `outputs` into the rows of `batch` that haven't changed, and advances backfill `id` past the batch.
pub(super) fn commit(
    connection: &Connection,
    schema: &DatabaseSchema,
    id: u64,
    batch: &Backfill,
    outputs: &[Value],
    (done, cursor): (u64, Option<&str>),
) -> Result<()> {
    if cursor != batch.cursor.as_deref() || outputs.len() != batch.rows.len() {
        return Ok(());
    }
    let (migration, table) = (batch.migration.as_str(), batch.table.as_str());
    let applied = journal::load(connection)?;
    let expand = applied.iter().find(|entry| entry.migration.id == migration).ok_or(Error::Corrupt("backfill"))?;
    let added = &expand.migration.tables.get(table).ok_or(Error::Corrupt("backfill"))?.added;
    let declared = expand.migration.schema.get(table).ok_or(Error::Corrupt("backfill"))?;
    let stored = schema.get(table).ok_or(Error::Corrupt("backfill"))?;
    let current = select(table, stored.fields.keys(), "WHERE _id = ?1");
    let assignments: Vec<_> = added.iter().map(|field| format!("{} = ?", quote(field))).collect();
    let update = format!("UPDATE {} SET _bytes = ?, {} WHERE _id = ?", quote(table), assignments.join(", "));
    let mut total: i64 =
        connection.query_row("SELECT document_bytes FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?;
    for (row, output) in batch.rows.iter().zip(outputs) {
        let latest = connection.prepare_cached(&current)?.query_row([&row.id], |latest| {
            let (revision, bytes): (u64, usize) = (latest.get(1)?, latest.get(2)?);
            Ok((revision, bytes, decode(latest, stored.fields.iter())))
        });
        let (revision, old_bytes, fields) = match latest {
            Ok(latest) => latest,
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(error) => return Err(error.into()),
        };
        if revision != row.revision {
            continue;
        }
        let mut fields = fields?;
        for field in added {
            match output.get(field) {
                Some(value) => fields.insert(field.clone(), value.clone()),
                None => fields.remove(field),
            };
        }
        let projected: Map<_, _> = fields
            .iter()
            .filter(|(name, _)| declared.fields.contains_key(*name))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let projected = Value::Object(projected);
        if !declared.accepts(&projected) || chunk_contract::validate_wire_value(&projected).is_err() {
            return Err(Error::Migration(format!("migration {migration} produced an invalid {table} row {}", row.id)));
        }
        let document = serde_json::to_vec(&fields)?.len();
        if document > MAX_DOCUMENT_BYTES {
            return Err(Error::Migration(format!("migration {migration} made {table} row {} too large", row.id)));
        }
        let mut values = vec![SqlValue::Integer(i64::try_from(document).map_err(|_| Error::Capacity)?)];
        for field in added {
            let definition = stored.fields.get(field).ok_or(Error::Corrupt("backfill"))?;
            values.push(codec::encode(definition, fields.get(field))?);
        }
        values.push(SqlValue::Text(row.id.clone()));
        connection.prepare_cached(&update)?.execute(params_from_iter(values))?;
        total += i64::try_from(document).map_err(|_| Error::Capacity)?
            - i64::try_from(old_bytes).map_err(|_| Error::Capacity)?;
    }
    if usize::try_from(total).map_err(|_| Error::Corrupt("document byte count"))? > MAX_DOCUMENT_TOTAL_BYTES {
        return Err(Error::Capacity);
    }
    connection.execute("UPDATE _chunk_metadata SET document_bytes = ?1 WHERE singleton = 1", [total])?;
    if !batch.complete {
        let last = &batch.rows[batch.rows.len() - 1].id;
        return work::advance(connection, id, done + batch.rows.len() as u64, last);
    }
    work::finish(connection, id)?;
    journal::schedule_drops(connection)
}
