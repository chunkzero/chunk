//! The migration journal in `server/migrations/`: `NNNN_<name>.ts` sources, `meta/journal.json` and a
//! `meta/NNNN.snapshot.json` of the full schema after each entry.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use chunk_contract::{DatabaseSchema, MigrationKind, MigrationTable};

mod diff;
mod journal;
mod source;
pub use source::Renames;

pub(crate) use journal::Journal;
use journal::{Entry, Lock};

/// Schema changes since the last snapshot that need a migration.
pub struct Pending {
    project: PathBuf,
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
    let journal = Journal::read(&project)?;
    journal.verify()?;
    let schema = crate::compiler::schema(&project)?;
    Ok(pending_from(project, journal, schema))
}

fn pending_from(project: PathBuf, journal: Journal, schema: DatabaseSchema) -> Pending {
    let changes = diff::changes(&journal.schema(), &schema);
    Pending { project, journal, schema, changes }
}

/// Writes the next migration for `pending`, or a baseline of the current schema when there is no history yet,
/// and returns its ID.
/// # Errors
/// Rejects invalid names or renames, and a schema with nothing to migrate.
pub fn create(pending: Pending, name: &str, renames: &Renames) -> io::Result<String> {
    let Pending { project, mut journal, schema, changes } = pending;
    journal::require_name(name)?;
    let _lock = Lock::acquire(&project)?;
    journal.require_unchanged(&project)?;
    journal.require_listed(true)?;
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
    let source =
        (kind == MigrationKind::Expand).then(|| source::migration(&id, &journal.schema(), &schema, &changes, renames));
    let entry = Entry {
        id: id.clone(),
        kind,
        finishes: None,
        prev: String::new(),
        replaced: BTreeMap::new(),
        hash: String::new(),
    };
    let mut snapshot = schema;
    for (table, shape) in journal.schema() {
        snapshot.entry(table).or_insert(shape);
    }
    journal.push(entry, snapshot, source.as_deref())?;
    journal.remove_leftovers()?;
    crate::sdk::generate_sdk(&project)?;
    Ok(id)
}

/// Writes the entry that drops the old shape of expand migration `number`, and returns its ID.
/// # Errors
/// Rejects an invalid journal and a migration that isn't an unfinished expand.
pub fn finish(project: &Path, number: &str) -> io::Result<String> {
    let _lock = Lock::acquire(project)?;
    let mut journal = verified(project, true)?;
    let target = journal.entries[journal.find(number)?].clone();
    if target.kind != MigrationKind::Expand
        || journal.entries.iter().any(|entry| entry.finishes.as_ref() == Some(&target.id))
    {
        return Err(io::Error::other(format!("{} is not an unfinished expand migration", target.id)));
    }
    let name = target.id.split_once('_').map_or("", |(_, name)| name);
    let id = journal.next_id(&journal::finish_name(name))?;
    let entry = Entry {
        id: id.clone(),
        kind: MigrationKind::Finish,
        finishes: Some(target.id),
        prev: String::new(),
        replaced: BTreeMap::new(),
        hash: String::new(),
    };
    let schema = journal.schema();
    journal.push(entry, schema, None)?;
    journal.remove_leftovers()?;
    crate::sdk::generate_sdk(project)?;
    Ok(id)
}

/// Verifies the journal: numbering, chaining, hashes and unlisted files.
/// # Errors
/// Describes the first conflict found.
pub fn check(project: &Path) -> io::Result<()> {
    verified(project, false).map(drop)
}

/// Records the current hash of migration `number`, after it was edited before being deployed.
/// # Errors
/// Rejects an unknown migration.
pub fn rehash(project: &Path, number: &str) -> io::Result<String> {
    let _lock = Lock::acquire(project)?;
    let mut journal = Journal::read(project)?;
    journal.require_listed(true)?;
    let index = journal.find(number)?;
    let hash = journal.current_hash(index);
    if let Some(next) = journal.entries.get_mut(index + 1) {
        next.prev.clone_from(&hash);
    }
    journal.entries[index].hash = hash;
    journal.verify()?;
    journal.save()?;
    journal.remove_leftovers()?;
    crate::sdk::generate_sdk(project)?;
    Ok(journal.entries[index].id.clone())
}

/// Replaces the longest prefix of history whose expands are all finished with one baseline, and returns its ID.
/// # Errors
/// Rejects an invalid journal and history with nothing finished.
pub fn squash(project: &Path) -> io::Result<String> {
    let _lock = Lock::acquire(project)?;
    let mut journal = verified(project, true)?;
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
    let rest_sources = journal.sources.split_off(end);
    journal.snapshots.clear();
    journal.sources.clear();
    let number = journal::number(&squashed[end - 1].id);
    let id = format!("{number}_baseline");
    let mut entry = Entry {
        id: id.clone(),
        kind: MigrationKind::Baseline,
        finishes: None,
        prev: String::new(),
        replaced: BTreeMap::new(),
        hash: String::new(),
    };
    entry.replaced = replaced_files(&journal.directory, &squashed)?;
    journal.append(entry, schema, None);
    for mut entry in rest {
        entry.prev = journal.entries.last().map(|last| last.hash.clone()).unwrap_or_default();
        journal.entries.push(entry);
    }
    journal.snapshots.extend(rest_snapshots);
    journal.sources.extend(rest_sources);
    journal.verify()?;
    journal.write_entry(0)?;
    journal.save()?;
    journal.remove_leftovers()?;
    crate::sdk::generate_sdk(project)?;
    Ok(id)
}

/// The hash of each source and snapshot file squashing `entries` leaves behind. The last snapshot is overwritten by
/// the baseline's.
fn replaced_files(directory: &Path, entries: &[Entry]) -> io::Result<BTreeMap<String, String>> {
    let mut files = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.kind == MigrationKind::Expand {
            files.push(format!("{}.ts", entry.id));
        }
        if index + 1 < entries.len() {
            files.push(format!("meta/{}.snapshot.json", journal::number(&entry.id)));
        }
    }
    files
        .into_iter()
        .map(|file| Ok((file.clone(), journal::file_hash(&std::fs::read(directory.join(&file))?))))
        .collect()
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

pub(crate) fn declarations(journal: &Journal) -> String {
    source::declarations(journal)
}

/// Reads the journal and checks its files and hashes. `repair` tolerates a squash's leftovers, which the command
/// holding the lock deletes once it succeeds.
pub(crate) fn verified(project: &Path, repair: bool) -> io::Result<Journal> {
    let journal = Journal::read(project)?;
    journal.require_listed(repair)?;
    journal.verify()?;
    Ok(journal)
}

#[cfg(test)]
mod tests;
