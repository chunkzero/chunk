//! Recomputes the bytes the documents of a table are charged after columns are dropped.

use rusqlite::{Connection, params, types::Value as SqlValue};
use serde_json::Map;

use crate::{Error, Result};
use chunk_contract::TableSchema;

use super::codec::{self, quote};

const BATCH: usize = 1024;

/// Updates each row's `_bytes` of `table`, which has `schema`'s fields, and the aggregate document bytes.
pub(super) fn run(connection: &Connection, table: &str, schema: &TableSchema) -> Result<()> {
    let mut columns = vec!["_id".to_owned(), "_bytes".to_owned()];
    columns.extend(schema.fields.keys().map(|field| quote(field)));
    let select = format!("SELECT {} FROM {} WHERE _id > ?1 ORDER BY _id LIMIT ?2", columns.join(", "), quote(table));
    let update = format!("UPDATE {} SET _bytes = ?1 WHERE _id = ?2", quote(table));
    let (mut cursor, mut delta) = (String::new(), 0_i64);
    loop {
        let mut counted = Vec::new();
        let mut statement = connection.prepare_cached(&select)?;
        let mut rows = statement.query(params![cursor, BATCH])?;
        while let Some(row) = rows.next()? {
            let (id, old): (String, i64) = (row.get(0)?, row.get(1)?);
            let mut document = Map::new();
            for (index, (name, field)) in schema.fields.iter().enumerate() {
                if let Some(value) = codec::decode(field, row.get::<_, SqlValue>(index + 2)?)? {
                    document.insert(name.clone(), value);
                }
            }
            let bytes = i64::try_from(serde_json::to_vec(&document)?.len()).map_err(|_| Error::Capacity)?;
            delta += bytes - old;
            counted.push((id, bytes));
        }
        drop(rows);
        for (id, bytes) in &counted {
            connection.prepare_cached(&update)?.execute(params![bytes, id])?;
        }
        match counted.last() {
            Some((last, _)) if counted.len() == BATCH => cursor.clone_from(last),
            _ => break,
        }
    }
    connection
        .execute("UPDATE _chunk_metadata SET document_bytes = document_bytes + ?1 WHERE singleton = 1", [delta])?;
    Ok(())
}
