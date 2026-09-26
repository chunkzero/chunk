//! Asynchronous replication of the environment log to object storage.
//!
//! A replicated store appends one entry per write transaction to `_chunk_log`
//! inside that transaction: its log sequence, the revision after it, any schema
//! DDL it ran and an SQLite session changeset of its row changes. Commits are
//! acknowledged after the local fsync; an uploader thread batches pending entries
//! into segments and uploads them once they are `batch_delay` old or
//! `batch_bytes` large, and only when entries exist.
//!
//! Object keys, relative to the configured prefix, with zero-padded decimals:
//!
//! - `epochs/{epoch}/snapshots/{sequence}.db`: a `VACUUM INTO` copy of the
//!   database containing every entry up to `sequence`.
//! - `epochs/{epoch}/segments/{first}-{last}.log`: entries `first..=last`.
//!
//! Every epoch starts with a snapshot, and a new snapshot replaces segments
//! whenever the local log has a gap (writes made without replication or a store
//! format migration) or after `snapshot_segments` segments or `snapshot_bytes`
//! bytes of segments. Restore takes the newest epoch's latest snapshot, replays
//! its later segments and continues under the next unused epoch. A writer
//! checks for the next epoch's claim before and after each upload, on every
//! flush and every `fence_interval` while idle; once it exists, the writer is
//! fenced and its commits and flushes fail.

use std::{
    io,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crate::{Error, Result};

mod restore;
mod s3;
mod segment;
mod uploader;

pub(crate) use restore::{Remote, restore};
pub(crate) use segment::Entry;

/// An S3-compatible bucket, or a stand-in for tests. Keys use `/` separators.
pub trait ObjectStorage: Send + Sync {
    /// # Errors
    /// Reports transport or storage failures.
    fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<()>;

    /// Stores `bytes` only if `key` is absent, atomically, reporting whether it did.
    /// # Errors
    /// Reports transport or storage failures.
    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool>;

    /// # Errors
    /// Reports missing objects, transport or storage failures.
    fn get(&self, key: &str) -> io::Result<Vec<u8>>;

    /// Lists every key below the `prefix` directory with its size in bytes.
    /// # Errors
    /// Reports transport or storage failures.
    fn list(&self, prefix: &str) -> io::Result<Vec<(String, u64)>>;
}

pub struct Replication {
    storage: Arc<dyn ObjectStorage>,
    batch_delay: Duration,
    batch_bytes: usize,
    snapshot_segments: usize,
    snapshot_bytes: u64,
    fence_interval: Duration,
}

impl Replication {
    /// Uploads within about five seconds of a commit, snapshots after 720
    /// segments or 64 MiB of segments and checks ownership every 10 seconds.
    #[must_use]
    pub fn new(storage: Arc<dyn ObjectStorage>) -> Self {
        Self {
            storage,
            batch_delay: Duration::from_secs(5),
            batch_bytes: 8 * 1024 * 1024,
            snapshot_segments: 720,
            snapshot_bytes: 64 * 1024 * 1024,
            fence_interval: Duration::from_secs(10),
        }
    }

    /// Reads S3-compatible storage settings. Replication is off unless
    /// `CHUNK_REPLICATION_BUCKET` is set; it then requires
    /// `CHUNK_REPLICATION_ACCESS_KEY_ID` and `CHUNK_REPLICATION_SECRET_ACCESS_KEY`
    /// and reads the optional `CHUNK_REPLICATION_ENDPOINT`, `CHUNK_REPLICATION_REGION`
    /// (default `us-east-1`) and `CHUNK_REPLICATION_PREFIX`.
    /// # Errors
    /// Rejects incomplete or invalid settings.
    pub fn from_env() -> Result<Option<Self>> {
        Ok(s3::S3::from_env()?.map(|storage| Self::new(Arc::new(storage))))
    }

    pub(crate) fn storage(&self) -> &dyn ObjectStorage {
        self.storage.as_ref()
    }
}

/// Owns the uploader thread. Dropping it stops uploading without flushing.
pub struct Replicator {
    shared: Arc<Shared>,
    storage: Arc<dyn ObjectStorage>,
    epoch: u64,
    thread: Option<JoinHandle<()>>,
}

impl Replicator {
    pub(crate) fn start(
        path: PathBuf,
        replication: Replication,
        epoch: u64,
        remote: &Remote,
        committed: u64,
    ) -> Result<(Self, Arc<Shared>)> {
        let stored = remote.uploaded(epoch);
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                committed,
                uploaded: stored,
                pending_since: (committed > stored).then(Instant::now),
                ..State::default()
            }),
            changed: Condvar::new(),
        });
        let storage = replication.storage.clone();
        let worker = uploader::Uploader::new(path, replication, epoch, remote, shared.clone())?;
        let thread = std::thread::Builder::new().name("chunk-replication".into()).spawn(move || worker.run())?;
        Ok((Self { shared: shared.clone(), storage, epoch, thread: Some(thread) }, shared))
    }

    /// Blocks until every transaction committed before the call is in object
    /// storage. Suspend and graceful shutdown call this after the last commit.
    /// # Errors
    /// Reports the first failed upload attempt after the call; the uploader keeps
    /// retrying in the background, so callers may flush again.
    /// Fails with [`Error::Fenced`] once another store claimed a newer epoch,
    /// which it checks in object storage before reporting success.
    pub fn flush(&self) -> Result<()> {
        self.uploaded()?;
        if fenced(self.storage.as_ref(), self.epoch)? {
            self.shared.fence();
            return Err(Error::Fenced);
        }
        Ok(())
    }

    fn uploaded(&self) -> Result<()> {
        let mut state = self.shared.lock();
        let target = state.committed;
        let failures = state.failures;
        if state.ended == Some(Ended::Fenced) {
            return Err(Error::Fenced);
        }
        if state.uploaded >= target {
            return Ok(());
        }
        state.flush = true;
        self.shared.changed.notify_all();
        while state.uploaded < target {
            match state.ended {
                Some(Ended::Fenced) => return Err(Error::Fenced),
                Some(Ended::Stopped) => return Err(Error::Replication("uploader stopped".into())),
                None => {}
            }
            if state.failures != failures {
                return Err(Error::Replication(state.error.clone()));
            }
            state = self.shared.changed.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
        Ok(())
    }

    /// Whether another store claimed a newer epoch. A fenced store's writes are
    /// no longer replicated, so its process should stop serving.
    #[must_use]
    pub fn fenced(&self) -> bool {
        self.shared.fenced()
    }
}

impl Drop for Replicator {
    fn drop(&mut self) {
        self.shared.lock().stop = true;
        self.shared.changed.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Coordinates the store, the uploader and flush callers. Nobody holds the
/// lock during I/O, so commits never wait on replication.
pub(crate) struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// The last local log sequence, including sequences without entries.
    committed: u64,
    /// The last sequence contained in this epoch's objects.
    uploaded: u64,
    pending_since: Option<Instant>,
    pending_bytes: usize,
    flush: bool,
    failures: u64,
    error: String,
    stop: bool,
    ended: Option<Ended>,
}

/// Why the uploader thread exited.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Another store claimed a newer epoch.
    Fenced,
    Stopped,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn uploaded(&self) -> u64 {
        self.lock().uploaded
    }

    pub(crate) fn fenced(&self) -> bool {
        self.lock().ended == Some(Ended::Fenced)
    }

    /// Stops the uploader and fails later commits and flushes.
    fn fence(&self) {
        self.lock().ended = Some(Ended::Fenced);
        self.changed.notify_all();
    }

    pub(crate) fn committed(&self, sequence: u64, bytes: usize) {
        let mut state = self.lock();
        state.committed = sequence;
        state.pending_bytes += bytes;
        state.pending_since.get_or_insert_with(Instant::now);
        self.changed.notify_all();
    }
}

/// Whether a newer epoch was claimed. Epochs are claimed in order, so any newer
/// claim implies the next one.
fn fenced(storage: &dyn ObjectStorage, epoch: u64) -> Result<bool> {
    Ok(!storage.list(&segment::epoch_key(epoch + 1))?.is_empty())
}

/// Owns `epoch` through its claim object, creating it when absent.
pub(crate) fn claim(storage: &dyn ObjectStorage, epoch: u64, token: &str) -> Result<()> {
    let key = segment::claim_key(epoch);
    if storage.create(&key, token.as_bytes().to_vec())? || storage.get(&key)? == token.as_bytes() {
        return Ok(());
    }
    Err(Error::StaleReplica)
}

/// Creates a file only its owner can read, since it holds environment data.
fn create_private(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}

/// A scratch file beside the database, on the same filesystem for atomic renames.
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(format!(".{suffix}"));
    name.into()
}

#[cfg(test)]
mod tests;
