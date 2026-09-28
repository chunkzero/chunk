use std::{
    collections::BTreeMap,
    fs,
    path::PathBuf,
    sync::{Mutex, PoisonError},
};

/// A release archive core downloaded from management and verified before activating the release. Its bytes are not
/// checked again on lookup, so readers must check them against `sha256` and `size` as they read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseArchive {
    pub path: PathBuf,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

/// The archives of the releases core keeps, by release ID.
#[derive(Default)]
pub(crate) struct Archives {
    kept: Mutex<BTreeMap<String, ReleaseArchive>>,
    #[cfg(test)]
    pub(crate) stall: std::sync::Arc<Stall>,
}

/// Held by tests to stall archive reads in their blocking I/O.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Stall {
    pub held: tokio::sync::Mutex<()>,
    /// Notified as each read reaches the stall.
    pub entered: tokio::sync::Notify,
}

impl Archives {
    /// Release `release`'s archive, while its file is still there with the size it was verified at.
    pub(crate) fn get(&self, release: &str) -> Option<ReleaseArchive> {
        let archive = self.lock().get(release).cloned()?;
        let present = fs::metadata(&archive.path).is_ok_and(|file| file.is_file() && file.len() == archive.size);
        present.then_some(archive)
    }

    pub(crate) fn insert(&self, release: String, archive: ReleaseArchive) {
        self.lock().insert(release, archive);
    }

    pub(crate) fn remove(&self, release: &str) {
        self.lock().remove(release);
    }

    /// Forgets the archives of the releases `keep` rejects.
    pub(crate) fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.lock().retain(|release, _| keep(release));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReleaseArchive>> {
        self.kept.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
