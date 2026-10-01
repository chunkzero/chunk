//! The migration journal in `server/migrations/`: `NNNN_<name>.ts` sources, `meta/journal.json` and a
//! `meta/NNNN.snapshot.json` of the full schema after each entry.

use std::{collections::BTreeMap, io, path::Path};

use chunk_contract::{DatabaseSchema, MigrationKind, MigrationTable};

mod diff;
mod journal;
mod source;
pub use source::Renames;

use journal::Entry;
pub(crate) use journal::Journal;

/// Schema changes since the last snapshot that need a migration.
pub struct Pending {
    journal: Journal,
    schema: DatabaseSchema,
    changes: BTreeMap<String, MigrationTable>,
}

impl Pending {
    /// Tables that need a migration, with their added and removed fields.
    #[must_use]
    pub fn changes(&self) -> &BTreeMap<String, MigrationTable> {
        &self.changes
    }
}

/// Diffs `server/schema/` against the last snapshot.
/// # Errors
/// Reports an invalid journal or schema.
pub fn pending(project: &Path) -> io::Result<Pending> {
    let project = project.canonicalize()?;
    let journal = verified(&project)?;
    let schema = crate::compiler::schema(&project)?;
    Ok(pending_from(journal, schema))
}

fn pending_from(journal: Journal, schema: DatabaseSchema) -> Pending {
    let changes = diff::changes(&journal.schema(), &schema);
    Pending { journal, schema, changes }
}

/// Writes the next migration for `pending`, or a baseline of the current schema when there is no history yet,
/// and returns its ID.
/// # Errors
/// Rejects invalid names or renames, and a schema with nothing to migrate.
pub fn create(pending: Pending, name: &str, renames: &Renames) -> io::Result<String> {
    let Pending { mut journal, schema, changes } = pending;
    let id = journal.next_id(name)?;
    for (table, fields) in renames {
        let change = changes.get(table).ok_or_else(|| io::Error::other(format!("{table} has no fields to rename")))?;
        for (old, new) in fields {
            if !change.removed.contains(old) || !change.added.contains(new) {
                return Err(io::Error::other(format!("{table}.{old} can't be renamed to {new}")));
            }
        }
    }
    let kind = if journal.entries.is_empty() {
        MigrationKind::Baseline
    } else if changes.is_empty() {
        return Err(io::Error::other(
            "server/schema/ has no changes that need a migration; additive changes apply automatically",
        ));
    } else {
        MigrationKind::Expand
    };
    let source = (kind == MigrationKind::Expand).then(|| source::migration(&id, &schema, &changes, renames));
    let entry = Entry { id: id.clone(), kind, finishes: None, prev: String::new(), hash: String::new() };
    journal.push(entry, schema, source.as_deref())?;
    Ok(id)
}

/// Writes the entry that drops the old shape of expand migration `number`, and returns its ID.
/// # Errors
/// Rejects an invalid journal and a migration that isn't an unfinished expand.
pub fn finish(project: &Path, number: &str) -> io::Result<String> {
    let mut journal = verified(project)?;
    let target = journal.entries[journal.find(number)?].clone();
    if target.kind != MigrationKind::Expand
        || journal.entries.iter().any(|entry| entry.finishes.as_ref() == Some(&target.id))
    {
        return Err(io::Error::other(format!("{} is not an unfinished expand migration", target.id)));
    }
    let name = target.id.split_once('_').map_or("", |(_, name)| name);
    let id = journal.next_id(&format!("finish_{name}"))?;
    let entry = Entry {
        id: id.clone(),
        kind: MigrationKind::Finish,
        finishes: Some(target.id),
        prev: String::new(),
        hash: String::new(),
    };
    let schema = journal.schema();
    journal.push(entry, schema, None)?;
    Ok(id)
}

/// Verifies the journal: numbering, chaining, hashes and unlisted files.
/// # Errors
/// Describes the first conflict found.
pub fn check(project: &Path) -> io::Result<()> {
    verified(project).map(drop)
}

/// Records the current hash of migration `number`, after it was edited before being deployed.
/// # Errors
/// Rejects an unknown migration.
pub fn rehash(project: &Path, number: &str) -> io::Result<String> {
    let mut journal = Journal::read(project)?;
    let index = journal.find(number)?;
    let hash = journal.current_hash(index)?;
    if let Some(next) = journal.entries.get_mut(index + 1) {
        next.prev.clone_from(&hash);
    }
    journal.entries[index].hash = hash;
    journal.save()?;
    journal.verify()?;
    Ok(journal.entries[index].id.clone())
}

/// Replaces the longest prefix of history whose expands are all finished with one baseline, and returns its ID.
/// # Errors
/// Rejects an invalid journal and history with nothing finished.
pub fn squash(project: &Path) -> io::Result<String> {
    let mut journal = verified(project)?;
    let mut open = 0_usize;
    let mut end = None;
    for (index, entry) in journal.entries.iter().enumerate() {
        match entry.kind {
            MigrationKind::Expand => open += 1,
            MigrationKind::Finish => open -= 1,
            MigrationKind::Baseline => {}
        }
        if open == 0 && index > 0 {
            end = Some(index + 1);
        }
    }
    let end = end.ok_or_else(|| io::Error::other("no finished migrations to squash"))?;
    let rest = journal.entries.split_off(end);
    let squashed = std::mem::take(&mut journal.entries);
    let schema = journal.snapshots[end - 1].clone();
    let rest_snapshots = journal.snapshots.split_off(end);
    journal.snapshots.clear();
    for entry in &squashed {
        if entry.kind == MigrationKind::Expand {
            std::fs::remove_file(journal.source_path(&entry.id))?;
        }
    }
    for entry in &squashed[..end - 1] {
        let number = entry.id.split('_').next().unwrap_or_default();
        std::fs::remove_file(journal.directory.join(format!("meta/{number}.snapshot.json")))?;
    }
    let number = squashed[end - 1].id.split('_').next().unwrap_or_default();
    let id = format!("{number}_baseline");
    let entry = Entry {
        id: id.clone(),
        kind: MigrationKind::Baseline,
        finishes: None,
        prev: String::new(),
        hash: String::new(),
    };
    journal.push(entry, schema, None)?;
    for mut entry in rest {
        entry.prev = journal.entries.last().map(|last| last.hash.clone()).unwrap_or_default();
        journal.entries.push(entry);
    }
    journal.snapshots.extend(rest_snapshots);
    journal.save()?;
    journal.verify()?;
    Ok(id)
}

/// Fails unless the last snapshot reaches `schema` through changes that need no migration.
pub(crate) fn require_replayed(journal: &Journal, schema: &DatabaseSchema) -> io::Result<()> {
    let changes = diff::changes(&journal.schema(), schema);
    if changes.is_empty() {
        return Ok(());
    }
    let since = journal.entries.last().map_or_else(String::new, |entry| format!(" since migration {}", entry.id));
    Err(io::Error::other(format!(
        "server/schema/ changed{since} in ways that need a migration: {}. Run `chunk migrate new <name>`",
        diff::describe(&changes)
    )))
}

pub(crate) fn declarations(project: &Path) -> io::Result<String> {
    Ok(source::declarations(&Journal::read(project)?))
}

fn verified(project: &Path) -> io::Result<Journal> {
    let journal = Journal::read(project)?;
    journal.verify()?;
    Ok(journal)
}

#[cfg(test)]
mod tests;
