use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension};

use crate::{DatabaseSchema, Error, Result, TableSchema};

use super::codec::{column, quote};

pub(super) fn load(connection: &Connection) -> Result<DatabaseSchema> {
    let schema: Option<String> = connection
        .query_row(
            "SELECT schema FROM _chunk_migrations ORDER BY revision DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    schema.map_or_else(
        || Ok(DatabaseSchema::new()),
        |schema| merge(&DatabaseSchema::new(), &serde_json::from_str(&schema)?),
    )
}

pub(super) fn merge(current: &DatabaseSchema, incoming: &DatabaseSchema) -> Result<DatabaseSchema> {
    let mut merged = current.clone();
    for (name, table) in incoming {
        chunk_contract::validate_name(name).map_err(Error::Invalid)?;
        table.validate().map_err(Error::Invalid)?;
        let Some(existing) = merged.get_mut(name) else {
            merged.insert(name.clone(), table.clone());
            continue;
        };
        for (field, definition) in &table.fields {
            match existing.fields.get(field) {
                Some(old) if old != definition => {
                    return Err(Error::Invalid("changing a field requires an explicit migration"));
                }
                Some(_) => {}
                None if !definition.optional => return Err(Error::Invalid("added fields must be optional")),
                None => {
                    existing.fields.insert(field.clone(), definition.clone());
                }
            }
        }
        for (index, fields) in &table.indexes {
            if existing.indexes.get(index).is_some_and(|old| old != fields) {
                return Err(Error::Invalid("changing an index requires an explicit migration"));
            }
            existing.indexes.insert(index.clone(), fields.clone());
        }
    }
    if merged.len() > 128 || serde_json::to_vec(&merged)?.len() > 1024 * 1024 {
        return Err(Error::Invalid("schema size limit"));
    }
    let mut names = BTreeSet::new();
    for (name, table) in &merged {
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(Error::Invalid("table names differ only by case"));
        }
        table.validate().map_err(Error::Invalid)?;
    }
    Ok(merged)
}

pub(super) fn apply(connection: &Connection, current: &DatabaseSchema, next: &DatabaseSchema) -> Result<()> {
    for (name, table) in next {
        let previous = current.get(name);
        if let Some(previous) = previous {
            for (field, definition) in &table.fields {
                if !previous.fields.contains_key(field) {
                    connection.execute_batch(&format!(
                        "ALTER TABLE {} ADD COLUMN {}",
                        quote(name),
                        column(field, definition)
                    ))?;
                }
            }
        } else {
            let mut columns = vec![
                "_id TEXT PRIMARY KEY".to_owned(),
                "_revision INTEGER NOT NULL CHECK (_revision > 0)".to_owned(),
                "_bytes INTEGER NOT NULL CHECK (_bytes >= 0)".to_owned(),
            ];
            columns.extend(table.fields.iter().map(|(name, field)| column(name, field)));
            connection.execute_batch(&format!(
                "CREATE TABLE {} ({}) STRICT, WITHOUT ROWID",
                quote(name),
                columns.join(", ")
            ))?;
        }
        for (index, fields) in &table.indexes {
            if previous.is_some_and(|p| p.indexes.contains_key(index)) {
                continue;
            }
            let mut columns: Vec<_> = fields.iter().map(|field| quote(field)).collect();
            columns.push("_id".into());
            connection.execute_batch(&format!(
                "CREATE INDEX {} ON {} ({})",
                quote(&index_name(name, index)),
                quote(name),
                columns.join(", ")
            ))?;
        }
    }
    Ok(())
}

pub(super) fn index_name(table: &str, index: &str) -> String {
    format!("_chunk_index_{}_{table}_{index}", table.len())
}

pub(super) fn select(table: &TableSchema) -> String {
    let mut columns = vec!["_id".to_owned(), "_revision".to_owned()];
    columns.extend(table.fields.keys().map(|name| quote(name)));
    columns.join(", ")
}

pub(super) fn upsert(name: &str, table: &TableSchema) -> String {
    let mut columns = vec!["_id".to_owned(), "_revision".to_owned(), "_bytes".to_owned()];
    columns.extend(table.fields.keys().map(|name| quote(name)));
    let placeholders = vec!["?"; columns.len()].join(", ");
    let update: Vec<_> = columns
        .iter()
        .skip(1)
        .map(|name| format!("{name} = excluded.{name}"))
        .collect();
    format!(
        "INSERT INTO {} ({}) VALUES ({placeholders}) ON CONFLICT (_id) DO UPDATE SET {}",
        quote(name),
        columns.join(", "),
        update.join(", ")
    )
}
