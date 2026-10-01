use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{IndexDefinition, PendingWork, Result, Work};

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

pub(super) fn get(connection: &Connection, id: u64) -> Result<Option<Work>> {
    let work: Option<String> =
        connection.query_row("SELECT work FROM _chunk_work WHERE id = ?1", [id], |row| row.get(0)).optional()?;
    Ok(work.map(|work| serde_json::from_str(&work)).transpose()?)
}

/// Records `work` unless it is already pending.
pub(super) fn record(connection: &Connection, work: impl IntoIterator<Item = Work>) -> Result<()> {
    for work in work {
        connection.execute(
            "INSERT OR IGNORE INTO _chunk_work (work, done, total) VALUES (?1, 0, 1)",
            params![serde_json::to_string(&work)?],
        )?;
    }
    Ok(())
}

pub(super) fn finish(connection: &Connection, id: u64) -> Result<()> {
    connection.execute("DELETE FROM _chunk_work WHERE id = ?1", [id])?;
    Ok(())
}

/// Removes index builds no longer `needed`.
pub(super) fn prune(connection: &Connection, needed: &BTreeSet<IndexDefinition>) -> Result<()> {
    for pending in load(connection)? {
        let Work::Index(index) = &pending.work;
        if !needed.contains(index) {
            finish(connection, pending.id)?;
        }
    }
    Ok(())
}
