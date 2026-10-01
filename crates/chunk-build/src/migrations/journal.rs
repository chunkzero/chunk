use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

use chunk_contract::{DatabaseSchema, Migration, MigrationKind, migration_number};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project::{Child, children};

const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Entry {
    pub id: String,
    pub kind: MigrationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finishes: Option<String>,
    /// The previous entry's hash, so entries written on different branches don't chain.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prev: String,
    pub hash: String,
}

/// `server/migrations/meta/journal.json` with each entry's snapshot.
pub(crate) struct Journal {
    pub directory: PathBuf,
    pub entries: Vec<Entry>,
    pub snapshots: Vec<DatabaseSchema>,
}

impl Journal {
    /// Reads the journal and its snapshots without verifying them; a project without one has an empty journal.
    pub fn read(project: &Path) -> io::Result<Self> {
        let directory = project.join("server/migrations");
        let path = directory.join("meta/journal.json");
        let document: Document = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| invalid(&path, error))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => Document { version: VERSION, entries: Vec::new() },
            Err(error) => return Err(error),
        };
        if document.version != VERSION {
            return Err(invalid(&path, "unsupported journal version"));
        }
        let mut snapshots = Vec::new();
        for entry in &document.entries {
            let path = snapshot_path(&directory, &entry.id);
            let bytes = fs::read(&path).map_err(|error| invalid(&path, error))?;
            snapshots.push(serde_json::from_slice(&bytes).map_err(|error| invalid(&path, error))?);
        }
        Ok(Self { directory, entries: document.entries, snapshots })
    }

    pub fn schema(&self) -> DatabaseSchema {
        self.snapshots.last().cloned().unwrap_or_default()
    }

    pub fn previous(&self, index: usize) -> DatabaseSchema {
        index.checked_sub(1).map(|index| self.snapshots[index].clone()).unwrap_or_default()
    }

    pub fn source_path(&self, id: &str) -> PathBuf {
        self.directory.join(format!("{id}.ts"))
    }

    pub fn find(&self, number: &str) -> io::Result<usize> {
        let wanted = number.parse::<u32>().ok();
        self.entries
            .iter()
            .position(|entry| wanted.is_some() && migration_number(&entry.id).map(|(_, n)| n) == wanted)
            .ok_or_else(|| io::Error::other(format!("no migration numbered {number}")))
    }

    /// The ID of the entry after the last, such as `0004_name`.
    pub fn next_id(&self, name: &str) -> io::Result<String> {
        let number = self.entries.last().and_then(|entry| migration_number(&entry.id)).map_or(1, |(_, n)| n + 1);
        let id = format!("{number:04}_{name}");
        if name.len() > 64 || migration_number(&id).is_none() {
            return Err(io::Error::other("migration names use lowercase letters, digits and underscores"));
        }
        Ok(id)
    }

    /// Appends an entry, writing its snapshot and, for an expand, its source.
    pub fn push(&mut self, mut entry: Entry, snapshot: DatabaseSchema, source: Option<&str>) -> io::Result<()> {
        if let Some(source) = source {
            write(&self.source_path(&entry.id), source.as_bytes())?;
        }
        entry.prev = self.entries.last().map(|last| last.hash.clone()).unwrap_or_default();
        entry.hash = hash(&entry, &snapshot, source.unwrap_or_default());
        write(&snapshot_path(&self.directory, &entry.id), &pretty(&snapshot)?)?;
        self.entries.push(entry);
        self.snapshots.push(snapshot);
        self.save()
    }

    pub fn save(&self) -> io::Result<()> {
        let document = Document { version: VERSION, entries: self.entries.clone() };
        write(&self.directory.join("meta/journal.json"), &pretty(&document)?)
    }

    /// The hash entry `index` should record for its files as they are now.
    pub fn current_hash(&self, index: usize) -> io::Result<String> {
        let entry = &self.entries[index];
        let source = match entry.kind {
            MigrationKind::Expand => {
                let path = self.source_path(&entry.id);
                fs::read_to_string(&path).map_err(|error| invalid(&path, error))?
            }
            _ => String::new(),
        };
        Ok(hash(entry, &self.snapshots[index], &source))
    }

    /// Checks that entries are numbered once, chain, match their recorded hashes and account for every file.
    pub fn verify(&self) -> io::Result<()> {
        let mut numbers = BTreeMap::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let number = number(&entry.id);
            if let Some(other) = numbers.insert(number, &entry.id) {
                return Err(io::Error::other(format!(
                    "migrations {other} and {} share number {number}, likely from different branches. Keep the one \
                     that may be deployed, remove the other's files and journal entry, and run `chunk migrate new` \
                     again",
                    entry.id
                )));
            }
            let previous = index.checked_sub(1).map(|index| self.entries[index].hash.as_str()).unwrap_or_default();
            if entry.prev != previous {
                return Err(io::Error::other(format!(
                    "migration {} was written on a different history than the migrations before it. Remove its files \
                     and journal entry and run `chunk migrate` again",
                    entry.id
                )));
            }
            if self.current_hash(index)? != entry.hash {
                return Err(io::Error::other(format!(
                    "migration {} no longer matches its recorded hash. Deployed migrations can't change; if it was \
                     never deployed, run `chunk migrate rehash {number}`",
                    entry.id
                )));
            }
        }
        self.unlisted()?;
        chunk_contract::validate_migrations(&self.contract(&BTreeMap::new()))
            .map_err(|error| invalid(&self.directory.join("meta/journal.json"), error))
    }

    /// Rejects migration sources and snapshots the journal doesn't list, such as ones merged from another branch.
    fn unlisted(&self) -> io::Result<()> {
        let sources: BTreeSet<_> = self
            .entries
            .iter()
            .filter(|entry| entry.kind == MigrationKind::Expand)
            .map(|entry| format!("{}.ts", entry.id))
            .collect();
        let snapshots: BTreeSet<_> =
            self.entries.iter().map(|entry| format!("{}.snapshot.json", number(&entry.id))).collect();
        let listed =
            children(&self.directory, "migration", |_| false)?.into_iter().map(|child| (child, &sources)).chain(
                children(&self.directory.join("meta"), "migration", |_| false)?.into_iter().map(|c| (c, &snapshots)),
            );
        for (Child { name, path, kind }, known) in listed {
            let extension = Path::new(&name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
            let tracked = matches!(extension.as_deref(), Some("ts" | "mts")) || name.ends_with(".snapshot.json");
            if kind.is_file() && tracked && !known.contains(&name) {
                return Err(invalid(&path, "not in meta/journal.json; was it merged from another branch?"));
            }
        }
        Ok(())
    }

    /// The contract's journal. `backs` lists, per migration and table, whether it has a `back` transform.
    pub fn contract(&self, backs: &BTreeMap<String, BTreeMap<String, bool>>) -> Vec<Migration> {
        let mut migrations: Vec<Migration> = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let tables = match entry.kind {
                MigrationKind::Expand => {
                    let mut tables = super::diff::changes(&self.previous(index), &self.snapshots[index]);
                    for (table, change) in &mut tables {
                        change.back =
                            backs.get(&entry.id).and_then(|tables| tables.get(table)).copied().unwrap_or(false);
                    }
                    tables
                }
                MigrationKind::Finish => migrations
                    .iter()
                    .find(|migration| Some(&migration.id) == entry.finishes.as_ref())
                    .map(|expand| {
                        expand
                            .tables
                            .iter()
                            .map(|(table, change)| {
                                (
                                    table.clone(),
                                    chunk_contract::MigrationTable {
                                        removed: change.removed.clone(),
                                        ..Default::default()
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                MigrationKind::Baseline => BTreeMap::new(),
            };
            migrations.push(Migration {
                id: entry.id.clone(),
                hash: entry.hash.clone(),
                kind: entry.kind,
                finishes: entry.finishes.clone(),
                tables,
                schema: self.snapshots[index].clone(),
            });
        }
        migrations
    }
}

fn number(id: &str) -> &str {
    id.split('_').next().unwrap_or(id)
}

fn snapshot_path(directory: &Path, id: &str) -> PathBuf {
    directory.join("meta").join(format!("{}.snapshot.json", number(id)))
}

pub(crate) fn hash(entry: &Entry, snapshot: &DatabaseSchema, source: &str) -> String {
    let kind = serde_json::to_value(entry.kind).expect("kind serialization");
    let mut digest = Sha256::new();
    digest.update(format!(
        "chunk-migration-v1\n{}\n{}\n{}\n",
        entry.id,
        kind.as_str().unwrap_or_default(),
        entry.finishes.as_deref().unwrap_or_default()
    ));
    digest.update(serde_json::to_vec(snapshot).expect("schema serialization"));
    digest.update(b"\n");
    digest.update(source.replace("\r\n", "\n"));
    format!("{:x}", digest.finalize())
}

fn pretty(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, bytes)
}

fn invalid(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
}
