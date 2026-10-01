use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{IndexDefinition, PendingWork, Result, Work};

use super::codec::quote;

pub(super) fn load(connection: &Connection) -> Result<Vec<PendingWork>> {
    let mut statement = connection.prepare("SELECT id, work, done, total FROM _chunk_work ORDER BY id")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, u64>(0)?, row.get::<_, String>(1)?, row.get::<_, u64>(2)?, row.get::<_, u64>(3)?))
    })?;
    rows.map(|row| {
        let (id, work, done, total) = row?;
        Ok(PendingWork { id, work: serde_json::from_str(&work)?, done, total })
    })
    .collect()
}

/// Pending work `id`, with its progress and backfill cursor.
pub(super) fn get(connection: &Connection, id: u64) -> Result<Option<(Work, u64, Option<String>)>> {
    let row: Option<(String, u64, Option<String>)> = connection
        .query_row("SELECT work, done, cursor FROM _chunk_work WHERE id = ?1", [id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    row.map(|(work, done, cursor)| Ok((serde_json::from_str(&work)?, done, cursor))).transpose()
}

/// Records `work` unless it is already pending. A backfill's total is its table's row count.
pub(super) fn record(connection: &Connection, work: impl IntoIterator<Item = Work>) -> Result<()> {
    for work in work {
        let total: u64 = match &work {
            Work::Backfill { table, .. } => {
                connection.query_row(&format!("SELECT count(*) FROM {}", quote(table)), [], |row| row.get(0))?
            }
            Work::Index(_) | Work::Drop { .. } => 1,
        };
        connection.execute(
            "INSERT OR IGNORE INTO _chunk_work (work, done, total) VALUES (?1, 0, ?2)",
            params![serde_json::to_string(&work)?, total],
        )?;
    }
    Ok(())
}

/// Records a backfill's progress: rows `done` so far, through ID `cursor`.
pub(super) fn advance(connection: &Connection, id: u64, done: u64, cursor: &str) -> Result<()> {
    connection.execute(
        "UPDATE _chunk_work SET done = ?2, total = max(total, ?2), cursor = ?3 WHERE id = ?1",
        params![id, done, cursor],
    )?;
    Ok(())
}

pub(super) fn finish(connection: &Connection, id: u64) -> Result<()> {
    connection.execute("DELETE FROM _chunk_work WHERE id = ?1", [id])?;
    Ok(())
}

/// Removes index builds no longer `needed`.
pub(super) fn prune(connection: &Connection, needed: &BTreeSet<IndexDefinition>) -> Result<()> {
    for pending in load(connection)? {
        if let Work::Index(index) = &pending.work
            && !needed.contains(index)
        {
            finish(connection, pending.id)?;
        }
    }
    Ok(())
}
