//! The records core keeps in its state directory across a restart, written atomically: the activation management has
//! not yet accepted, which keeps the deployment it replaced protected, and the deployments to stop.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeSet,
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

/// The deployments that stop at once instead of draining, as of control's `current` deployment.
#[derive(Clone, Default, Serialize, Deserialize)]
pub(super) struct Stopping {
    pub current: String,
    pub deployments: BTreeSet<String>,
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
