use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension};

use crate::{DatabaseSchema, Error, IndexDefinition, Result};

use super::{
    codec::{column, quote},
    indexes,
};

pub(super) fn load(connection: &Connection) -> Result<DatabaseSchema> {
    let schema: Option<String> = connection
        .query_row("SELECT schema FROM _chunk_migrations ORDER BY revision DESC LIMIT 1", [], |row| row.get(0))
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
    /// Indexes built by the statements.
    pub indexes: Vec<IndexDefinition>,
}

/// Adds `incoming`'s tables and optional fields to `current`. With the indexes
/// already `built`, `incoming`'s indexes join the schema too, and the statements
/// build those not built yet. Without, its indexes are left to pending work.
pub(super) fn merge(
    current: &DatabaseSchema,
    incoming: &DatabaseSchema,
    built: Option<&BTreeSet<IndexDefinition>>,
) -> Result<Migration> {
    let mut merged = current.clone();
    let mut statements = Vec::new();
    for (name, table) in incoming {
        let existing = merged.entry(name.clone()).or_insert_with(|| {
            statements.push(create(name, &table.fields));
            chunk_contract::TableSchema { fields: table.fields.clone(), indexes: BTreeMap::new() }
        });
        for (field, definition) in &table.fields {
            match existing.fields.get(field) {
                Some(old) if old != definition => {
                    return Err(Error::Invalid("changing a field requires an explicit migration"));
                }
                Some(_) => {}
                None if !definition.optional => return Err(Error::Invalid("added fields must be optional")),
                None => {
                    statements.push(format!("ALTER TABLE {} ADD COLUMN {}", quote(name), column(field, definition)));
                    existing.fields.insert(field.clone(), definition.clone());
                }
            }
        }
        if built.is_none() {
            continue;
        }
        for (index, fields) in &table.indexes {
            match existing.indexes.get(index) {
                Some(old) if old != fields => {
                    return Err(Error::Invalid("changing a schema's index requires an explicit migration"));
                }
                Some(_) => {}
                None => {
                    existing.indexes.insert(index.clone(), fields.clone());
                }
            }
        }
    }
    chunk_contract::validate(&merged).map_err(Error::Invalid)?;
    let indexes: Vec<_> = built.map_or_else(Vec::new, |built| {
        IndexDefinition::declared(incoming).filter(|index| !built.contains(index)).collect()
    });
    statements.extend(indexes.iter().map(indexes::create));
    Ok(Migration { schema: merged, statements, indexes })
}

pub(super) fn create(name: &str, fields: &BTreeMap<String, chunk_contract::Field>) -> String {
    let mut columns = vec![
        "_id TEXT PRIMARY KEY".to_owned(),
        "_revision INTEGER NOT NULL CHECK (_revision > 0)".to_owned(),
        "_bytes INTEGER NOT NULL CHECK (_bytes >= 0)".to_owned(),
    ];
    columns.extend(fields.iter().map(|(name, field)| column(name, field)));
    format!("CREATE TABLE {} ({}) STRICT, WITHOUT ROWID", quote(name), columns.join(", "))
}

/// Replaces the schema recorded at the current revision, for changes that leave documents as they are.
pub(super) fn replace(transaction: &rusqlite::Transaction<'_>, schema: &DatabaseSchema) -> Result<()> {
    let revision = super::revision::current(transaction)?;
    transaction.execute(
        "INSERT OR REPLACE INTO _chunk_migrations (revision, schema) VALUES (?1, ?2)",
        rusqlite::params![revision, serde_json::to_string(schema)?],
    )?;
    Ok(())
}

pub(super) fn install(
    transaction: &rusqlite::Transaction<'_>,
    current: &DatabaseSchema,
    migration: &Migration,
) -> Result<crate::Revision> {
    let revision = super::revision::current(transaction)?;
    if migration.statements.is_empty() && migration.schema == *current {
        return Ok(revision);
    }
    let next = super::revision::next(revision)?;
    for statement in &migration.statements {
        transaction.execute_batch(statement)?;
    }
    indexes::record(transaction, &migration.indexes)?;
    transaction.execute(
        "INSERT INTO _chunk_migrations (revision, schema) VALUES (?1, ?2)",
        rusqlite::params![next, serde_json::to_string(&migration.schema)?],
    )?;
    transaction.execute("UPDATE _chunk_metadata SET revision = ?1 WHERE singleton = 1", [next])?;
    Ok(next)
}
