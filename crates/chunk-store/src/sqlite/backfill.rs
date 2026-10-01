//! Backfills apply an expand migration's `to` to a table in batches ordered by ID. Each batch reads the latest rows
//! in the write that updates them, so a document written before the backfill reaches it is transformed as it is
//! now.

use rusqlite::{Connection, params, params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value};

use crate::{DatabaseSchema, Error, MAX_DOCUMENT_BYTES, Result, Transform};

use super::{
    codec::{self, quote},
    journal, work,
    write::MAX_DOCUMENT_TOTAL_BYTES,
};

const BATCH: usize = 256;
/// Bounds the documents one batch holds in memory.
const BATCH_BYTES: usize = 512 * 1024;

type Row = (String, usize, Map<String, Value>);

/// Reads up to `limit` rows of `table` after `cursor`, and how many it read: it stops early at the batch's byte
/// bound, so a count equal to the rows returned means no row was left behind.
fn read(
    connection: &Connection,
    table: &str,
    stored: &chunk_contract::TableSchema,
    (cursor, limit): (Option<&str>, usize),
) -> Result<(Vec<Row>, usize)> {
    let mut columns = vec!["_id".to_owned(), "_bytes".to_owned()];
    columns.extend(stored.fields.keys().map(|field| quote(field)));
    let sql = format!("SELECT {} FROM {} WHERE _id > ?1 ORDER BY _id LIMIT ?2", columns.join(", "), quote(table));
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut statement = connection.prepare(&sql)?;
    let mut query = statement.query(params![cursor.unwrap_or(""), limit])?;
    let mut read = 0;
    while let Some(row) = query.next()? {
        read += 1;
        let (row_id, old_bytes): (String, usize) = (row.get(0)?, row.get(1)?);
        if !rows.is_empty() && bytes + old_bytes > BATCH_BYTES {
            break;
        }
        bytes += old_bytes;
        let mut fields = Map::new();
        for (index, (name, field)) in stored.fields.iter().enumerate() {
            if let Some(value) = codec::decode(field, row.get::<_, SqlValue>(index + 2)?)? {
                fields.insert(name.clone(), value);
            }
        }
        rows.push((row_id, old_bytes, fields));
    }
    drop(query);
    Ok((rows, read))
}

/// Applies the next batch of backfill `id`, one transform call per row, finishing it once no rows remain.
pub(super) fn run(
    connection: &Connection,
    schema: &DatabaseSchema,
    id: u64,
    (migration, table): (&str, &str),
    (done, cursor): (u64, Option<&str>),
    transform: &mut Transform<'_>,
) -> Result<()> {
    let applied = journal::load(connection)?;
    let expand = applied.iter().find(|entry| entry.migration.id == migration).ok_or(Error::Corrupt("backfill"))?;
    let added = &expand.migration.tables.get(table).ok_or(Error::Corrupt("backfill"))?.added;
    let declared = expand.migration.schema.get(table).ok_or(Error::Corrupt("backfill"))?;
    let stored = schema.get(table).ok_or(Error::Corrupt("backfill"))?;
    let (rows, read) = read(connection, table, stored, (cursor, BATCH))?;
    if rows.is_empty() {
        work::finish(connection, id)?;
        return journal::schedule_drops(connection);
    }
    let failed =
        |reason: String| Error::Migration(format!("migration {migration} failed to backfill {table}: {reason}"));
    let readable = expand.migration.input_fields(table, false);
    let mut outputs = Vec::with_capacity(rows.len());
    for (row_id, _, fields) in &rows {
        let mut input: Map<_, _> = fields
            .iter()
            .filter(|(name, _)| readable.contains(name.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        input.insert("_id".into(), Value::String(row_id.clone()));
        outputs.push(
            transform(migration, table, Value::Object(input))
                .map_err(|reason| failed(format!("row {row_id}: {reason}")))?,
        );
    }
    let (finished, last, count) =
        (read < BATCH && rows.len() == read, rows[rows.len() - 1].0.clone(), rows.len() as u64);
    let assignments: Vec<_> = added.iter().map(|field| format!("{} = ?", quote(field))).collect();
    let update = format!("UPDATE {} SET _bytes = ?, {} WHERE _id = ?", quote(table), assignments.join(", "));
    let mut total: i64 =
        connection.query_row("SELECT document_bytes FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?;
    for ((row_id, old_bytes, mut fields), output) in rows.into_iter().zip(outputs) {
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
        if !declared.accepts(&Value::Object(projected)) {
            return Err(Error::Migration(format!("migration {migration} produced an invalid {table} row {row_id}")));
        }
        let document = serde_json::to_vec(&fields)?.len();
        if document > MAX_DOCUMENT_BYTES {
            return Err(Error::Migration(format!("migration {migration} made {table} row {row_id} too large")));
        }
        let mut values = vec![SqlValue::Integer(i64::try_from(document).map_err(|_| Error::Capacity)?)];
        for field in added {
            let definition = stored.fields.get(field).ok_or(Error::Corrupt("backfill"))?;
            values.push(codec::encode(definition, fields.get(field))?);
        }
        values.push(SqlValue::Text(row_id));
        connection.prepare_cached(&update)?.execute(params_from_iter(values))?;
        total += i64::try_from(document).map_err(|_| Error::Capacity)?
            - i64::try_from(old_bytes).map_err(|_| Error::Capacity)?;
    }
    if usize::try_from(total).map_err(|_| Error::Corrupt("document byte count"))? > MAX_DOCUMENT_TOTAL_BYTES {
        return Err(Error::Capacity);
    }
    connection.execute("UPDATE _chunk_metadata SET document_bytes = ?1 WHERE singleton = 1", [total])?;
    if !finished {
        return work::advance(connection, id, done + count, &last);
    }
    work::finish(connection, id)?;
    journal::schedule_drops(connection)
}
