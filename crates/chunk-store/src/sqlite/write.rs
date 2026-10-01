use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chunk_contract::TableSchema;

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as SqlValue};

use crate::{Commit, DatabaseSchema, DocumentKey, Error, JobIntent, Operation, Outcome, Result, Revision};

use super::codec;
use super::codec::quote;

pub(super) const MAX_DOCUMENT_BYTES: usize = 1024 * 1024;
/// Each write charges its key and document JSON, so deletes count too.
const MAX_COMMIT_BYTES: usize = 64 * 1024 * 1024;
/// Keeps the database, and so its snapshots and restores, to a bounded size.
pub(super) const MAX_DOCUMENT_TOTAL_BYTES: usize = 16 * 1024 * 1024 * 1024;

struct PreparedWrite<'a> {
    key: &'a DocumentKey,
    upsert: Arc<str>,
    values: Option<Vec<SqlValue>>,
    bytes: usize,
}

pub(super) struct Prepared<'a> {
    writes: Vec<PreparedWrite<'a>>,
    pub result: String,
}

impl<'a> Prepared<'a> {
    pub fn new(commit: &'a Commit, schema: &DatabaseSchema) -> Result<Self> {
        let mut commit_bytes = 0;
        let mut keys = BTreeSet::new();
        let mut statements = BTreeMap::new();
        let mut writes = Vec::with_capacity(commit.writes.len());
        for write in &commit.writes {
            write.key.validate()?;
            if !keys.insert(&write.key) {
                return Err(Error::Invalid("duplicate document write"));
            }
            let table = schema.get(&write.key.table).ok_or(Error::Invalid("undeclared table"))?;
            let mut bytes = 0;
            let values = if let Some(value) = &write.value {
                if !value.as_object().is_some_and(|object| object.keys().all(|name| table.fields.contains_key(name))) {
                    return Err(Error::Invalid("document does not match table schema"));
                }
                bytes = serde_json::to_vec(value)?.len();
                if bytes > MAX_DOCUMENT_BYTES {
                    return Err(Error::Capacity);
                }
                commit_bytes += bytes;
                Some(
                    table
                        .fields
                        .iter()
                        .map(|(name, field)| codec::encode(field, value.get(name)))
                        .collect::<Result<_>>()?,
                )
            } else {
                None
            };
            commit_bytes += write.key.table.len() + write.key.id.len();
            if commit_bytes > MAX_COMMIT_BYTES {
                return Err(Error::Capacity);
            }
            writes.push(PreparedWrite {
                key: &write.key,
                upsert: statements
                    .entry(&write.key.table)
                    .or_insert_with(|| Arc::<str>::from(upsert(&write.key.table, table)))
                    .clone(),
                values,
                bytes,
            });
        }
        let result = serde_json::to_string(&commit.result)?;
        if result.len() > MAX_DOCUMENT_BYTES {
            return Err(Error::Capacity);
        }
        Ok(Self { writes, result })
    }

    /// Called inside the same transaction as the revision and operation outcome.
    pub fn apply(&self, connection: &Connection, next: Revision) -> Result<()> {
        let (mut count, mut bytes): (usize, usize) = connection.query_row(
            "SELECT document_count, document_bytes FROM _chunk_metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        for write in &self.writes {
            let table = codec::quote(&write.key.table);
            let old_bytes: Option<usize> = connection
                .prepare_cached(&format!("SELECT _bytes FROM {table} WHERE _id = ?"))?
                .query_row([&write.key.id], |row| row.get(0))
                .optional()?;
            count = count
                .checked_sub(usize::from(old_bytes.is_some()))
                .and_then(|count| count.checked_add(usize::from(write.values.is_some())))
                .ok_or(Error::Corrupt("document count"))?;
            bytes = bytes
                .checked_sub(old_bytes.unwrap_or(0))
                .and_then(|bytes| bytes.checked_add(write.bytes))
                .ok_or(Error::Corrupt("document byte count"))?;
            if let Some(values) = &write.values {
                let mut params: Vec<&dyn rusqlite::ToSql> = vec![&write.key.id, &next, &write.bytes];
                params.extend(values.iter().map(|value| value as &dyn rusqlite::ToSql));
                connection.prepare_cached(&write.upsert)?.execute(params_from_iter(params))?;
            } else {
                connection.prepare_cached(&format!("DELETE FROM {table} WHERE _id = ?"))?.execute([&write.key.id])?;
            }
        }
        if bytes > MAX_DOCUMENT_TOTAL_BYTES {
            return Err(Error::Capacity);
        }
        connection.execute(
            "UPDATE _chunk_metadata SET document_count = ?1, document_bytes = ?2 WHERE singleton = 1",
            params![count, bytes],
        )?;
        Ok(())
    }
}

/// Applies one commit inside the caller's transaction, stamping its outcome with `now`.
pub(super) fn commit(
    connection: &Connection,
    schema: &DatabaseSchema,
    commit: Commit,
    intents: &[JobIntent],
    limits: &super::jobs::JobLimits,
    now: i64,
) -> Result<Outcome> {
    if let Some(outcome) = outcome(connection, &commit.operation)? {
        return Ok(outcome);
    }
    let prepared = Prepared::new(&commit, schema)?;
    let current = super::revision::current(connection)?;
    if current != commit.expected {
        return Err(Error::Conflict { expected: commit.expected, actual: current });
    }
    let next = super::revision::next(current)?;
    prepared.apply(connection, next)?;
    super::jobs::apply(connection, intents, limits)?;
    connection.execute("UPDATE _chunk_metadata SET revision = ?1 WHERE singleton = 1", [next])?;
    connection.execute(
        "INSERT INTO _chunk_operations (operation_id, fingerprint, revision, result, committed_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![commit.operation.id, commit.operation.fingerprint.as_slice(), next, prepared.result, now],
    )?;
    connection.execute("DELETE FROM _chunk_retry_contexts WHERE operation_id = ?1", [&commit.operation.id])?;
    Ok(Outcome { revision: next, result: commit.result })
}

pub(super) fn outcome(connection: &Connection, operation: &Operation) -> Result<Option<Outcome>> {
    operation.validate()?;
    let record = connection
        .query_row(
            "SELECT fingerprint, revision, result FROM _chunk_operations WHERE operation_id = ?1",
            [&operation.id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Revision>(1)?, row.get::<_, String>(2)?)),
        )
        .optional()?;
    record
        .map(|(fingerprint, revision, result)| {
            if fingerprint != operation.fingerprint {
                return Err(Error::OperationMismatch);
            }
            Ok(Outcome { revision, result: serde_json::from_str(&result)? })
        })
        .transpose()
}

fn upsert(name: &str, table: &TableSchema) -> String {
    let mut columns = vec!["_id".to_owned(), "_revision".to_owned(), "_bytes".to_owned()];
    columns.extend(table.fields.keys().map(|name| quote(name)));
    let placeholders = vec!["?"; columns.len()].join(", ");
    let update: Vec<_> = columns.iter().skip(1).map(|name| format!("{name} = excluded.{name}")).collect();
    format!(
        "INSERT INTO {} ({}) VALUES ({placeholders}) ON CONFLICT (_id) DO UPDATE SET {}",
        quote(name),
        columns.join(", "),
        update.join(", ")
    )
}
