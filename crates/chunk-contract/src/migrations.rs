use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{DatabaseSchema, Field};

const MAX_MIGRATIONS: usize = 256;

/// One entry of a deployment's migration journal; `Contracts::migrations` lists them in order.
///
/// IDs are `NNNN_name`: at least four digits, then a lowercase name. An optional `namespace:` prefix is reserved
/// for companion modules; numbers increase within each namespace. `hash` is the SHA-256 the build recorded for the
/// entry's kind, snapshot and source.
///
/// - `Expand` changes the schema to `schema`. For each table in `tables`, `added` fields are new and `removed`
///   fields lose their old shape; a field in both changed its definition. The backend bundle exports
///   `__chunk_migrate(_, { migration, table, direction, rows })`, which maps each row through the migration's
///   `to` (`direction: "to"`, a row of the previous snapshot, returning exactly the `added` fields) or `back`
///   (`"back"`, a row of `schema`, returning exactly the `removed` fields) and returns the results in order. `back`
///   is present only where `MigrationTable::back` is set.
/// - `Finish` drops the old shape of the expand named by `finishes`. Its `tables` repeat that expand's `removed`
///   fields, and `schema` is unchanged.
/// - `Baseline` comes first in its namespace and stands in for every earlier entry numbered up to its own, whose
///   resulting schema is `schema`. It has no tables.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub id: String,
    pub hash: String,
    pub kind: MigrationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finishes: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tables: BTreeMap<String, MigrationTable>,
    pub schema: DatabaseSchema,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationKind {
    Expand,
    Finish,
    Baseline,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationTable {
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub back: bool,
}

/// Splits a migration ID into its namespace (empty without a prefix) and number.
#[must_use]
pub fn migration_number(id: &str) -> Option<(&str, u32)> {
    let (namespace, local) = id.split_once(':').unwrap_or(("", id));
    let (number, name) = local.split_once('_')?;
    let valid = (namespace.is_empty() || crate::deployment::identifier(namespace))
        && id.len() <= 128
        && number.len() >= 4
        && number.bytes().all(|b| b.is_ascii_digit())
        && !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    valid.then(|| number.parse().ok().map(|number| (namespace, number))).flatten()
}

/// # Errors
/// Rejects invalid IDs, misordered or duplicate entries, finishes without an open expand, invalid snapshots and
/// snapshots that differ from the previous one by more than the entry declares.
pub fn validate_migrations(migrations: &[Migration]) -> Result<(), &'static str> {
    if migrations.len() > MAX_MIGRATIONS {
        return Err("too many migrations");
    }
    let empty = DatabaseSchema::new();
    let mut last = BTreeMap::new();
    let mut previous: BTreeMap<&str, &DatabaseSchema> = BTreeMap::new();
    let mut open: BTreeMap<&str, &Migration> = BTreeMap::new();
    for migration in migrations {
        let (namespace, number) = migration_number(&migration.id).ok_or("invalid migration id")?;
        let first = !last.contains_key(namespace);
        if last.insert(namespace, number).is_some_and(|previous| previous >= number) {
            return Err("migrations must increase in number");
        }
        if migration.hash.len() != 64 || !migration.hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err("invalid migration hash");
        }
        crate::validate(&migration.schema)?;
        for (table, changes) in &migration.tables {
            crate::validate_name(table)?;
            changes.added.iter().chain(&changes.removed).try_for_each(|field| crate::validate_name(field))?;
        }
        let before = previous.insert(namespace, &migration.schema).unwrap_or(&empty);
        match migration.kind {
            MigrationKind::Expand => {
                if migration.finishes.is_some() || migration.tables.is_empty() {
                    return Err("an expand migration changes tables and finishes nothing");
                }
                require_declared_changes(before, migration)?;
                open.insert(migration.id.as_str(), migration);
            }
            MigrationKind::Finish => {
                let target = migration.finishes.as_deref().ok_or("a finish migration names its expand")?;
                let expand = open.remove(target).ok_or("a finish migration must follow its unfinished expand")?;
                if migration_number(target).map(|(space, _)| space) != Some(namespace) {
                    return Err("a finish migration must follow its unfinished expand");
                }
                if migration.schema != *before || !repeats_removed(expand, migration) {
                    return Err("a finish migration keeps the schema and repeats exactly its expand's removed fields");
                }
            }
            MigrationKind::Baseline => {
                if !first || migration.finishes.is_some() || !migration.tables.is_empty() {
                    return Err("a baseline comes first in its namespace and has no tables");
                }
            }
        }
    }
    Ok(())
}

fn repeats_removed(expand: &Migration, finish: &Migration) -> bool {
    finish.tables.len() == expand.tables.len()
        && expand.tables.iter().all(|(table, change)| {
            finish.tables.get(table).is_some_and(|repeated| {
                repeated.removed == change.removed && repeated.added.is_empty() && !repeated.back
            })
        })
}

/// Checks that `expand.schema` is `before` plus the declared `added` and `removed` fields of its tables, and
/// otherwise only new tables, new optional fields and index changes.
fn require_declared_changes(before: &DatabaseSchema, expand: &Migration) -> Result<(), &'static str> {
    const MESSAGE: &str = "an expand migration's snapshot must differ from the previous one only as declared";
    if expand.tables.keys().any(|table| !before.contains_key(table) || !expand.schema.contains_key(table)) {
        return Err("an expand migration's tables must exist in its snapshot and the previous one");
    }
    for (name, table) in &expand.schema {
        let Some(old) = before.get(name) else { continue };
        let differing = |from: &BTreeMap<String, Field>, to: &BTreeMap<String, Field>| -> Vec<String> {
            from.iter()
                .filter(|(field, definition)| to.get(*field) != Some(definition))
                .map(|(f, _)| f.clone())
                .collect()
        };
        let (added, removed) = (differing(&table.fields, &old.fields), differing(&old.fields, &table.fields));
        match expand.tables.get(name) {
            Some(declared) => {
                if declared.added != added || declared.removed != removed || (added.is_empty() && removed.is_empty()) {
                    return Err(MESSAGE);
                }
            }
            None => {
                if !removed.is_empty() || added.iter().any(|field| !table.fields[field].optional) {
                    return Err(MESSAGE);
                }
            }
        }
    }
    Ok(())
}
