use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Mutex, PoisonError},
};

/// A release archive core downloaded from management and verified before activating the release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseArchive {
    pub path: PathBuf,
    /// Lowercase hex.
    pub sha256: String,
    pub size: u64,
}

/// The archives of the releases core keeps, by release ID.
#[derive(Default)]
pub(crate) struct Archives(Mutex<BTreeMap<String, ReleaseArchive>>);

impl Archives {
    pub(crate) fn get(&self, release: &str) -> Option<ReleaseArchive> {
        self.lock().get(release).cloned()
    }

    pub(crate) fn insert(&self, release: String, archive: ReleaseArchive) {
        self.lock().insert(release, archive);
    }

    /// Forgets the archives of the releases `keep` rejects.
    pub(crate) fn retain(&self, keep: impl Fn(&str) -> bool) {
        self.lock().retain(|release, _| keep(release));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReleaseArchive>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
