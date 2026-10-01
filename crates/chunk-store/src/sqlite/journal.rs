//! The environment's applied migrations, and the schema they store.
//!
//! While an expand migration is active, its tables keep their removed fields and store its added fields as
//! optional, so deployments on either side of it can read and write. Dropping its old shape after it is finished
//! makes it inactive.

use std::collections::BTreeSet;

use chunk_contract::{DatabaseSchema, Deployment, Field, Migration, MigrationKind, TableSchema, migration_number};
use rusqlite::{Connection, params};

use crate::{Error, Result, Work};

use super::{codec::quote, deployments, schema, work};

#[derive(Clone)]
pub(super) struct Applied {
    pub migration: Migration,
    pub active: bool,
}

pub(super) fn load(connection: &Connection) -> Result<Vec<Applied>> {
    let mut statement = connection.prepare("SELECT migration, active FROM _chunk_applied ORDER BY position")?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)))?;
    rows.map(|row| {
        let (migration, active) = row?;
        Ok(Applied { migration: serde_json::from_str(&migration)?, active })
    })
    .collect()
}

/// The IDs of the expands in `applied` whose old shape is dropped.
pub(super) fn contracted(applied: &[Applied]) -> impl Iterator<Item = &str> {
    applied
        .iter()
        .filter(|entry| entry.migration.kind == MigrationKind::Expand && !entry.active)
        .map(|entry| entry.migration.id.as_str())
}

/// Appends `new` after the `applied` entries already recorded, with a backfill for each table an active expand
/// adds fields to.
pub(super) fn record(connection: &Connection, applied: usize, new: &[Applied]) -> Result<()> {
    for (position, Applied { migration, active }) in (applied..).zip(new) {
        connection.execute(
            "INSERT INTO _chunk_applied (position, id, migration, active) VALUES (?1, ?2, ?3, ?4)",
            params![position, migration.id, serde_json::to_string(migration)?, active],
        )?;
        let backfills = migration.tables.iter().filter(|(_, change)| *active && !change.added.is_empty());
        work::record(
            connection,
            backfills.map(|(table, _)| Work::Backfill { migration: migration.id.clone(), table: table.clone() }),
        )?;
    }
    Ok(())
}

/// Records the backfills of active expands in `deployment`'s journal that no deployment `carried` before it.
/// Without a carrier, writes left their added fields as they were.
pub(super) fn rebackfill(
    connection: &Connection,
    applied: &[Applied],
    deployment: &Deployment,
    carried: &BTreeSet<String>,
) -> Result<()> {
    let journal: BTreeSet<_> = deployment.contracts.migrations.iter().map(|migration| &migration.id).collect();
    let uncarried = applied.iter().filter(|entry| {
        entry.active && journal.contains(&entry.migration.id) && !carried.contains(&entry.migration.id)
    });
    let backfills: Vec<_> = uncarried
        .flat_map(|entry| {
            let tables = entry.migration.tables.iter().filter(|(_, change)| !change.added.is_empty());
            tables.map(|(table, _)| Work::Backfill { migration: entry.migration.id.clone(), table: table.clone() })
        })
        .collect();
    work::record(connection, backfills)
}

/// Checks `deployment`'s journal against `applied`, and plans the schema change that applies its new entries.
/// An expand that the new entries also finish is applied directly, with no work, when its tables have no rows.
pub(super) fn install(
    connection: &Connection,
    current: &DatabaseSchema,
    applied: &[Applied],
    deployment: &Deployment,
) -> Result<(schema::Migration, Vec<Applied>)> {
    let journal = &deployment.contracts.migrations;
    chunk_contract::validate_migrations(journal)
        .map_err(|error| Error::Migration(format!("invalid migration journal: {error}")))?;
    if let Some(migration) = crate::rolled_back_past(contracted(applied), journal) {
        return Err(crate::rollback_error(migration));
    }
    let new = verify(applied, journal)?;
    if !applied.is_empty() {
        require_stored(current, &stored(applied))?;
    }
    let direct = direct(connection, current, new, &deployments::load(connection)?)?;
    let new: Vec<_> = new
        .iter()
        .map(|migration| Applied {
            migration: migration.clone(),
            active: migration.kind == MigrationKind::Expand && !direct.contains(&migration.id),
        })
        .collect();
    let mut next = applied.to_vec();
    next.extend(new.iter().cloned());
    for (index, entry) in next.iter().enumerate().skip(applied.len()) {
        let tables = &entry.migration.tables;
        let busy = next[..index]
            .iter()
            .find(|earlier| earlier.active && earlier.migration.tables.keys().any(|table| tables.contains_key(table)));
        if let Some(earlier) = busy.filter(|_| entry.migration.kind == MigrationKind::Expand) {
            return Err(Error::Migration(format!(
                "migration {} changes a table that {} still migrates; deploy an intermediate release that completes {} \
                 first",
                entry.migration.id, earlier.migration.id, earlier.migration.id
            )));
        }
    }
    let migration = plan(current, &stored(&next), !direct.is_empty())?;
    let migrating = migrating(&next);
    for (name, table) in &deployment.tables {
        for (field, declared) in &table.fields {
            let stored = migration.schema.get(name).and_then(|table| table.fields.get(field));
            if !stored
                .is_some_and(|stored| crate::compatible_field(stored, declared, migrating.contains(&(name, field))))
            {
                return Err(Error::Migration(format!(
                    "deployment {} declares {name}.{field}, which the environment doesn't store as declared",
                    deployment.id
                )));
            }
        }
    }
    Ok((migration, new))
}

/// The IDs of the expands in `new` that a finish in `new` completes, all over tables with no rows, where no
/// resident deployment declares a removed field or lacks an added one.
fn direct(
    connection: &Connection,
    current: &DatabaseSchema,
    new: &[Migration],
    residents: &[Deployment],
) -> Result<BTreeSet<String>> {
    let mut direct = BTreeSet::new();
    for expand in new.iter().filter(|expand| new.iter().any(|finish| finish.finishes.as_ref() == Some(&expand.id))) {
        let mut empty = droppable(expand, residents);
        for table in expand.tables.keys().filter(|table| current.contains_key(*table)) {
            let rows = connection.query_row(&format!("SELECT EXISTS (SELECT 1 FROM {})", quote(table)), [], |row| {
                row.get::<_, bool>(0)
            })?;
            empty &= !rows;
        }
        if empty {
            direct.insert(expand.id.clone());
        }
    }
    Ok(direct)
}

/// The tables and fields active expands add or remove.
fn migrating(applied: &[Applied]) -> BTreeSet<(&String, &String)> {
    applied
        .iter()
        .filter(|applied| applied.active)
        .flat_map(|applied| &applied.migration.tables)
        .flat_map(|(table, change)| change.added.iter().chain(&change.removed).map(move |field| (table, field)))
        .collect()
}

/// The entries `journal` adds to `applied`, once the two are prefix-related with matching hashes. A baseline
/// stands in for the applied entries numbered up to its own, so it needs the environment to have reached it.
fn verify<'a>(applied: &[Applied], journal: &'a [Migration]) -> Result<&'a [Migration]> {
    let mut position = 0;
    for (index, entry) in journal.iter().enumerate() {
        let Some(current) = applied.get(position).map(|applied| &applied.migration) else {
            if entry.kind == MigrationKind::Baseline && !applied.is_empty() {
                return Err(Error::Migration(format!(
                    "baseline {} replaces migrations this environment hasn't applied",
                    entry.id
                )));
            }
            return Ok(&journal[index..]);
        };
        if current.id == entry.id {
            if current.hash != entry.hash {
                return Err(Error::Migration(format!(
                    "migration {} differs from the one this environment applied; applied migrations can't be edited",
                    entry.id
                )));
            }
            position += 1;
        } else if entry.kind == MigrationKind::Baseline {
            let (namespace, number) = migration_number(&entry.id).ok_or(Error::Corrupt("migration id"))?;
            let covered = applied[position..]
                .iter()
                .take_while(|applied| {
                    migration_number(&applied.migration.id).is_some_and(|(space, n)| space == namespace && n <= number)
                })
                .count();
            let last = covered.checked_sub(1).map(|last| &applied[position + last].migration);
            if last.is_none_or(|last| migration_number(&last.id) != Some((namespace, number))) {
                return Err(Error::Migration(format!(
                    "baseline {} replaces migrations this environment hasn't applied",
                    entry.id
                )));
            }
            if applied[position..position + covered].iter().any(|applied| applied.active) {
                return Err(Error::Migration(format!(
                    "baseline {} replaces migrations this environment hasn't completed; wait until their old \
                     shapes are dropped",
                    entry.id
                )));
            }
            if last.is_some_and(|last| last.schema != entry.schema) {
                return Err(Error::Migration(format!(
                    "baseline {} doesn't match the schema of the migrations it replaces",
                    entry.id
                )));
            }
            position += covered;
        } else {
            return Err(Error::Migration(format!(
                "migration {} conflicts with {}, which this environment applied",
                entry.id, current.id
            )));
        }
    }
    Ok(&[])
}

/// The schema `applied` stores: the last snapshot without indexes, with each active expand's removed fields kept
/// and its added fields optional.
pub(super) fn stored(applied: &[Applied]) -> DatabaseSchema {
    let Some(last) = applied.last() else { return DatabaseSchema::new() };
    let mut stored = last.migration.schema.clone();
    for table in stored.values_mut() {
        table.indexes.clear();
    }
    for (index, entry) in applied.iter().enumerate().filter(|(_, entry)| entry.active) {
        let previous = index.checked_sub(1).map(|previous| &applied[previous].migration.schema);
        for (name, change) in &entry.migration.tables {
            let Some(table) = stored.get_mut(name) else { continue };
            for field in &change.added {
                if let Some(field) = table.fields.get_mut(field) {
                    field.optional = true;
                }
            }
            for field in &change.removed {
                let old = previous.and_then(|schema| schema.get(name)).and_then(|table| table.fields.get(field));
                if let Some(old) = old {
                    table.fields.insert(field.clone(), Field { optional: true, ..old.clone() });
                }
            }
        }
    }
    stored
}

fn drift(table: &str, field: Option<&str>) -> Error {
    let at = field.map_or_else(|| table.to_owned(), |field| format!("{table}.{field}"));
    Error::Migration(format!("the stored schema doesn't match the environment's applied migrations at {at}"))
}

/// Requires `current`'s app tables to have exactly the fields of `stored`'s.
fn require_stored(current: &DatabaseSchema, stored: &DatabaseSchema) -> Result<()> {
    for (name, table) in current.iter().filter(|(name, _)| !crate::is_system_table(name)) {
        let wanted = stored.get(name).ok_or_else(|| drift(name, None))?;
        let differing = table.fields.iter().find(|(field, definition)| wanted.fields.get(*field) != Some(definition));
        let missing = wanted.fields.keys().find(|field| !table.fields.contains_key(*field));
        if let Some(field) = differing.map(|(field, _)| field).or(missing) {
            return Err(drift(name, Some(field)));
        }
    }
    match stored.keys().find(|name| !current.contains_key(*name)) {
        Some(name) => Err(drift(name, None)),
        None => Ok(()),
    }
}

/// Changes `current`'s app tables to `target`'s: creating tables, adding fields and changing whether they are
/// required. With `drop`, fields `target` lacks are dropped; otherwise they fail like any other difference.
pub(super) fn plan(current: &DatabaseSchema, target: &DatabaseSchema, drop: bool) -> Result<schema::Migration> {
    let mut merged = current.clone();
    let mut statements = Vec::new();
    for (name, table) in merged.iter_mut().filter(|(name, _)| !crate::is_system_table(name)) {
        let wanted = target.get(name).ok_or_else(|| drift(name, None))?;
        let dropped: Vec<_> =
            table.fields.keys().filter(|field| !wanted.fields.contains_key(*field)).cloned().collect();
        for field in dropped {
            if !drop {
                return Err(drift(name, Some(&field)));
            }
            statements.push(format!("ALTER TABLE {} DROP COLUMN {}", quote(name), quote(&field)));
            table.fields.remove(&field);
        }
    }
    for (name, wanted) in target {
        let table = merged.entry(name.clone()).or_insert_with(|| {
            statements.push(schema::create(name, &wanted.fields));
            TableSchema { fields: wanted.fields.clone(), indexes: std::collections::BTreeMap::new() }
        });
        for (field, definition) in &wanted.fields {
            match table.fields.get_mut(field) {
                None => {
                    statements.push(format!(
                        "ALTER TABLE {} ADD COLUMN {}",
                        quote(name),
                        super::codec::column(field, definition)
                    ));
                    table.fields.insert(field.clone(), definition.clone());
                }
                Some(stored) if stored.schema == definition.schema => stored.optional = definition.optional,
                Some(_) => {
                    return Err(Error::Migration(format!(
                        "a migration changes {name}.{field} in place, which isn't supported; rename the field instead"
                    )));
                }
            }
        }
    }
    chunk_contract::validate_physical(&merged).map_err(Error::Invalid)?;
    Ok(schema::Migration { schema: merged, statements, indexes: Vec::new() })
}

/// Whether `expand` can stop syncing and drop its old shape: every resident deployment that declares a changed
/// table declares all of the fields it adds and none of those it removes.
fn droppable(expand: &Migration, deployments: &[Deployment]) -> bool {
    expand.tables.iter().all(|(name, change)| {
        deployments.iter().filter_map(|deployment| deployment.tables.get(name)).all(|table| {
            change.added.iter().all(|field| table.fields.contains_key(field))
                && !change.removed.iter().any(|field| table.fields.contains_key(field))
        })
    })
}

/// The migrations with a backfill pending.
fn backfilling(connection: &Connection) -> Result<BTreeSet<String>> {
    let pending = work::load(connection)?;
    Ok(pending
        .into_iter()
        .filter_map(|pending| match pending.work {
            Work::Backfill { migration, .. } => Some(migration),
            _ => None,
        })
        .collect())
}

fn finished(applied: &[Applied], expand: &str) -> bool {
    applied.iter().any(|entry| entry.migration.finishes.as_deref() == Some(expand))
}

/// Records dropping each finished, backfilled expand whose old shape no resident deployment declares.
pub(super) fn schedule_drops(connection: &Connection) -> Result<()> {
    let applied = load(connection)?;
    let deployments = deployments::load(connection)?;
    let backfilling = backfilling(connection)?;
    let drops = applied.iter().filter(|entry| {
        let expand = &entry.migration;
        entry.active
            && finished(&applied, &expand.id)
            && !backfilling.contains(&expand.id)
            && droppable(expand, &deployments)
    });
    work::record(connection, drops.map(|entry| Work::Drop { migration: entry.migration.id.clone() }))
}

/// Plans dropping `expand`'s old shape from `current`, and lists the tables it changes, unless that can't happen
/// yet: a backfill is pending, or a deployment declares the old shape.
pub(super) fn drop_plan(
    connection: &Connection,
    current: &DatabaseSchema,
    expand: &str,
) -> Result<Option<(schema::Migration, Vec<String>)>> {
    let mut applied = load(connection)?;
    let deployments = deployments::load(connection)?;
    let Some(index) = applied.iter().position(|entry| entry.active && entry.migration.id == expand) else {
        return Ok(None);
    };
    if !finished(&applied, expand)
        || !droppable(&applied[index].migration, &deployments)
        || backfilling(connection)?.contains(expand)
    {
        return Ok(None);
    }
    applied[index].active = false;
    let tables = applied[index].migration.tables.keys().cloned().collect();
    Ok(Some((plan(current, &stored(&applied), true)?, tables)))
}

pub(super) fn deactivate(connection: &Connection, expand: &str) -> Result<()> {
    connection.execute("UPDATE _chunk_applied SET active = 0 WHERE id = ?1", [expand])?;
    Ok(())
}
