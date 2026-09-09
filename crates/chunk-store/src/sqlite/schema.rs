use rusqlite::{Connection, OptionalExtension};

use crate::{DatabaseSchema, Error, Result};

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
        |schema| {
            let schema = serde_json::from_str(&schema)?;
            chunk_contract::validate(&schema).map_err(Error::Invalid)?;
            Ok(schema)
        },
    )
}

pub(super) struct Migration {
    pub schema: DatabaseSchema,
    pub statements: Vec<String>,
}

pub(super) fn merge(current: &DatabaseSchema, incoming: &DatabaseSchema) -> Result<Migration> {
    let mut merged = current.clone();
    let mut statements = Vec::new();
    for (name, table) in incoming {
        if let Some(existing) = merged.get_mut(name) {
            for (field, definition) in &table.fields {
                match existing.fields.get(field) {
                    Some(old) if old != definition => {
                        return Err(Error::Invalid("changing a field requires an explicit migration"));
                    }
                    Some(_) => {}
                    None if !definition.optional => return Err(Error::Invalid("added fields must be optional")),
                    None => {
                        statements.push(format!(
                            "ALTER TABLE {} ADD COLUMN {}",
                            quote(name),
                            column(field, definition)
                        ));
                        existing.fields.insert(field.clone(), definition.clone());
                    }
                }
            }
            for (index, fields) in &table.indexes {
                match existing.indexes.get(index) {
                    Some(old) if old != fields => {
                        return Err(Error::Invalid("changing an index requires an explicit migration"));
                    }
                    Some(_) => {}
                    None => {
                        statements.push(create_index(name, index, fields));
                        existing.indexes.insert(index.clone(), fields.clone());
                    }
                }
            }
        } else {
            let mut columns = vec![
                "_id TEXT PRIMARY KEY".to_owned(),
                "_revision INTEGER NOT NULL CHECK (_revision > 0)".to_owned(),
                "_bytes INTEGER NOT NULL CHECK (_bytes >= 0)".to_owned(),
            ];
            columns.extend(table.fields.iter().map(|(name, field)| column(name, field)));
            statements.push(format!(
                "CREATE TABLE {} ({}) STRICT, WITHOUT ROWID",
                quote(name),
                columns.join(", ")
            ));
            for (index, fields) in &table.indexes {
                statements.push(create_index(name, index, fields));
            }
            merged.insert(name.clone(), table.clone());
        }
    }
    chunk_contract::validate(&merged).map_err(Error::Invalid)?;
    Ok(Migration {
        schema: merged,
        statements,
    })
}

fn create_index(table: &str, index: &str, fields: &[String]) -> String {
    let mut columns: Vec<_> = fields.iter().map(|field| quote(field)).collect();
    columns.push("_id".into());
    format!(
        "CREATE INDEX {} ON {} ({})",
        quote(&index_name(table, index)),
        quote(table),
        columns.join(", ")
    )
}

pub(super) fn index_name(table: &str, index: &str) -> String {
    format!("_chunk_index_{}_{table}_{index}", table.len())
}

pub(super) fn install(transaction: &rusqlite::Transaction<'_>, migration: &Migration) -> Result<crate::Revision> {
    let current = super::revision::current(transaction)?;
    if migration.statements.is_empty() {
        return Ok(current);
    }
    let next = super::revision::next(current)?;
    for statement in &migration.statements {
        transaction.execute_batch(statement)?;
    }
    transaction.execute(
        "INSERT INTO _chunk_migrations (revision, schema) VALUES (?1, ?2)",
        rusqlite::params![next, serde_json::to_string(&migration.schema)?],
    )?;
    transaction.execute("UPDATE _chunk_metadata SET revision = ?1 WHERE singleton = 1", [next])?;
    Ok(next)
}
