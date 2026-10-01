//! Physical indexes, one per definition, under hidden names.

use std::{collections::BTreeSet, fmt::Write};

use rusqlite::Connection;

use crate::{IndexDefinition, Result};

use super::codec::quote;

/// Built indexes.
pub(super) fn load(connection: &Connection) -> Result<BTreeSet<IndexDefinition>> {
    let mut statement = connection.prepare("SELECT definition FROM _chunk_indexes")?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    rows.map(|definition| Ok(serde_json::from_str(&definition?)?)).collect()
}

pub(super) fn record(connection: &Connection, built: &[IndexDefinition]) -> Result<()> {
    for index in built {
        connection.execute("INSERT INTO _chunk_indexes VALUES (?1)", [serde_json::to_string(index)?])?;
    }
    Ok(())
}

pub(super) fn forget(connection: &Connection, dropped: &[IndexDefinition]) -> Result<()> {
    for index in dropped {
        connection.execute("DELETE FROM _chunk_indexes WHERE definition = ?1", [serde_json::to_string(index)?])?;
    }
    Ok(())
}

/// Each part is length-prefixed, so distinct definitions never share a name.
pub(super) fn name(index: &IndexDefinition) -> String {
    let mut name = String::from("_chunk_index");
    for part in [&index.table, &index.name].into_iter().chain(&index.fields) {
        let _ = write!(name, "_{}_{part}", part.len());
    }
    name
}

pub(super) fn create(index: &IndexDefinition) -> String {
    let mut columns: Vec<_> = index.fields.iter().map(|field| quote(field)).collect();
    columns.push("_id".into());
    format!("CREATE INDEX {} ON {} ({})", quote(&name(index)), quote(&index.table), columns.join(", "))
}

pub(super) fn drop(index: &IndexDefinition) -> String {
    format!("DROP INDEX {}", quote(&name(index)))
}
