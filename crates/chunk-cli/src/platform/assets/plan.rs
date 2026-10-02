//! What differs between asset revisions, and what a pull or push may do about it.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use chunk_build::project::{ProjectMetadata, WorldFormat};
use chunk_contract::AssetRevision;

/// One world, pack or file of a revision.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Entry {
    /// An app's world: app ID and name.
    World(String, String),
    Pack(String),
    /// A file of the project's `assets/`.
    Shared(String),
    /// A file of an app's `assets/`: app ID and path.
    File(String, String),
}

impl Entry {
    fn kind(&self) -> &'static str {
        match self {
            Self::World(..) => "worlds",
            Self::Pack(_) => "packs",
            Self::Shared(_) | Self::File(..) => "files",
        }
    }
}

impl fmt::Display for Entry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::World(app, name) => write!(formatter, "world {app}/{name}"),
            Self::Pack(name) => write!(formatter, "pack {name}"),
            Self::Shared(path) => write!(formatter, "file {path}"),
            Self::File(app, path) => write!(formatter, "file {path} of {app}"),
        }
    }
}

/// The SHA-256 of every entry of the revision.
pub(super) fn entries(revision: &AssetRevision) -> BTreeMap<Entry, &str> {
    let mut entries = BTreeMap::new();
    for (name, pack) in &revision.packs {
        entries.insert(Entry::Pack(name.clone()), pack.sha256.as_str());
    }
    for (path, file) in &revision.shared {
        entries.insert(Entry::Shared(path.clone()), file.sha256.as_str());
    }
    for (app, assets) in &revision.apps {
        for (name, world) in &assets.worlds {
            entries.insert(Entry::World(app.clone(), name.clone()), world.sha256.as_str());
        }
        for (path, file) in &assets.files {
            entries.insert(Entry::File(app.clone(), path.clone()), file.sha256.as_str());
        }
    }
    entries
}

/// One line per kind of entry that differs, such as `worlds: 1 added, 2 changed`.
pub(super) fn summary(old: &BTreeMap<Entry, &str>, new: &BTreeMap<Entry, &str>) -> Vec<String> {
    let mut counts: BTreeMap<&str, [usize; 3]> = BTreeMap::new();
    for (entry, sha256) in new {
        match old.get(entry) {
            None => counts.entry(entry.kind()).or_default()[0] += 1,
            Some(before) if before != sha256 => counts.entry(entry.kind()).or_default()[1] += 1,
            Some(_) => {}
        }
    }
    for entry in old.keys().filter(|entry| !new.contains_key(*entry)) {
        counts.entry(entry.kind()).or_default()[2] += 1;
    }
    counts
        .into_iter()
        .map(|(kind, counts)| {
            let parts: Vec<String> = ["added", "changed", "removed"]
                .into_iter()
                .zip(counts)
                .filter(|(_, count)| *count > 0)
                .map(|(verb, count)| format!("{count} {verb}"))
                .collect();
            format!("{kind}: {}", parts.join(", "))
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Push {
    /// The built revision is the head.
    UpToDate,
    /// The head moved since the base.
    Moved,
    Send,
}

/// Whether a built revision may become the head, given the head and the base this checkout last synced to. Empty
/// IDs mean no head.
pub(super) fn check_push(head: &str, base: Option<&str>, built: &str, force: bool) -> Push {
    if head == built {
        Push::UpToDate
    } else if !head.is_empty() && base != Some(head) && !force {
        Push::Moved
    } else {
        Push::Send
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Pull {
    Keep,
    /// Replace the local file with the head's bytes.
    Write,
    Delete,
    /// Both sides changed the file; leave it.
    Conflict,
}

/// A three-way decision on one local file, given the SHA-256 of the entry in the base revision, in the head
/// revision and on disk; `None` for absent.
pub(super) fn decide(base: Option<&str>, head: Option<&str>, local: Option<&str>) -> Pull {
    if head == base || local == head {
        Pull::Keep
    } else if local == base {
        if head.is_some() { Pull::Write } else { Pull::Delete }
    } else {
        Pull::Conflict
    }
}

/// The local file an entry is built from, or why it can't be pulled into one.
pub(super) fn locate(entry: &Entry, root: &Path, metadata: &ProjectMetadata) -> Result<PathBuf, String> {
    let app =
        |id: &str| metadata.apps.iter().find(|app| app.id == id).ok_or_else(|| format!("this project has no app {id}"));
    match entry {
        Entry::Shared(path) => Ok(root.join("assets").join(path)),
        Entry::File(id, path) => Ok(root.join(&app(id)?.directory).join("assets").join(path)),
        Entry::World(id, name) => {
            let world =
                app(id)?.worlds.get(name).ok_or_else(|| format!("{id} declares no world {name} in this project"))?;
            match world.format {
                WorldFormat::Polar => Ok(root.join(&world.source)),
                WorldFormat::Anvil => Err(format!("built from the Anvil save {}", world.source)),
            }
        }
        Entry::Pack(name) => {
            let pack = metadata.apps.iter().find_map(|app| app.packs.get(name));
            let pack = pack.ok_or_else(|| format!("no app of this project declares pack {name}"))?;
            if Path::new(&pack.source).extension().is_some_and(|extension| extension.eq_ignore_ascii_case("zip")) {
                Ok(root.join(&pack.source))
            } else {
                Err(format!("built from the directory {}", pack.source))
            }
        }
    }
}
