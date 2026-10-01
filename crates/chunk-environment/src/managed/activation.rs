//! The record core keeps in its state directory across a restart, written atomically: the activation management has
//! not yet accepted, which keeps the deployment it replaced protected. It holds the committed value beside the one an
//! activation in progress would replace it with, so a crash before control commits that activation loses nothing.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{self, Write},
    path::Path,
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Activation {
    /// Control's current deployment before `activated`, which management may still fall back to.
    pub predecessor: Option<String>,
    pub activated: String,
}

/// A record's committed value, and the one an activation that control may not have committed yet would make.
#[derive(Serialize, Deserialize)]
pub(super) struct Records<T> {
    pub committed: Option<T>,
    pub pending: Option<T>,
}

impl<T: Serialize> Records<&T> {
    /// Durably records `self`, or removes the record when it holds neither.
    pub fn store(&self, path: &Path) -> io::Result<()> {
        if self.committed.is_none() && self.pending.is_none() { clear(path) } else { write(path, self) }
    }
}

/// The records at `path`, if any.
pub(super) fn recorded<T: DeserializeOwned>(path: &Path) -> io::Result<Records<T>> {
    Ok(read(path)?.unwrap_or(Records { committed: None, pending: None }))
}

/// The record at `path`, if any.
pub(super) fn read<T: DeserializeOwned>(path: &Path) -> io::Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Durably records `record` at `path`, replacing any earlier one.
pub(super) fn write(path: &Path, record: &impl Serialize) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let written = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&serde_json::to_vec(record).map_err(io::Error::other)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if written.is_err() {
        _ = fs::remove_file(&temporary);
    }
    written?;
    sync_parent(path)
}

/// Durably removes the record at `path`.
pub(super) fn clear(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        removed => removed.and_then(|()| sync_parent(path)),
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| io::Error::other("the record has no directory"))?;
    fs::File::open(parent)?.sync_all()
}
