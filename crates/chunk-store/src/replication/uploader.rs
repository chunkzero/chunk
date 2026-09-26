use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, PoisonError},
    time::{Duration, Instant},
};

use rusqlite::{Connection, OpenFlags};

use super::{Ended, Remote, Replication, Shared, segment};
use crate::{Error, Result, sqlite::log};

const SEGMENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub(super) struct Uploader {
    path: PathBuf,
    connection: Connection,
    replication: Replication,
    epoch: u64,
    shared: Arc<Shared>,
    /// Whether this epoch has a snapshot in storage.
    based: bool,
    /// Segments and their bytes uploaded since this epoch's latest snapshot.
    segments: usize,
    bytes: u64,
    /// When ownership was last checked in object storage.
    checked: Cell<Instant>,
}

enum Work {
    Upload { through: u64 },
    CheckOwnership,
}

impl Uploader {
    pub fn new(
        path: PathBuf,
        replication: Replication,
        epoch: u64,
        remote: &Remote,
        shared: Arc<Shared>,
    ) -> Result<Self> {
        let connection =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let (segments, bytes) = remote.since_snapshot(epoch);
        Ok(Self {
            path,
            connection,
            replication,
            epoch,
            shared,
            based: remote.base(epoch).is_some(),
            segments,
            bytes,
            checked: Cell::new(Instant::now()),
        })
    }

    pub fn run(mut self) {
        let shared = self.shared.clone();
        let _exit = Exit(&shared);
        let mut backoff = Duration::from_secs(1);
        while let Some(work) = self.wait() {
            let target = match work {
                Work::Upload { through } => through,
                Work::CheckOwnership => {
                    // Transient failures are retried after another interval.
                    if matches!(self.fence(), Err(Error::Fenced)) {
                        shared.fence();
                        return;
                    }
                    continue;
                }
            };
            // Progress counts only once no newer epoch claimed the environment by
            // the end of the upload, so a flush never reports superseded objects.
            let result = self.fence().and_then(|()| self.upload(target)).and_then(|reached| {
                self.fence()?;
                Ok(reached)
            });
            let error = match result {
                Ok(reached) => {
                    self.advance(reached);
                    backoff = Duration::from_secs(1);
                    continue;
                }
                Err(Error::Fenced) => {
                    shared.fence();
                    return;
                }
                Err(error) => error,
            };
            let mut state = shared.lock();
            state.failures += 1;
            state.error = error.to_string();
            shared.changed.notify_all();
            let (state, _) = shared
                .changed
                .wait_timeout_while(state, backoff, |state| !state.stop)
                .unwrap_or_else(PoisonError::into_inner);
            drop(state);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Waits until a batch or an ownership check is due.
    fn wait(&self) -> Option<Work> {
        let mut state = self.shared.lock();
        loop {
            if state.stop || state.ended.is_some() {
                return None;
            }
            let check = self.replication.fence_interval.saturating_sub(self.checked.get().elapsed());
            if check.is_zero() {
                return Some(Work::CheckOwnership);
            }
            if state.committed <= state.uploaded {
                state = self.shared.changed.wait_timeout(state, check).unwrap_or_else(PoisonError::into_inner).0;
                continue;
            }
            let waited = state.pending_since.map_or(self.replication.batch_delay, |since| since.elapsed());
            if state.flush
                || state.pending_bytes >= self.replication.batch_bytes
                || waited >= self.replication.batch_delay
            {
                return Some(Work::Upload { through: state.committed });
            }
            state = self
                .shared
                .changed
                .wait_timeout(state, self.replication.batch_delay.saturating_sub(waited).min(check))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Fails with [`Error::Fenced`] once a newer epoch is claimed.
    fn fence(&self) -> Result<()> {
        self.checked.set(Instant::now());
        if super::fenced(self.replication.storage.as_ref(), self.epoch)? { Err(Error::Fenced) } else { Ok(()) }
    }

    /// Uploads entries through `target` and returns the last sequence stored.
    fn upload(&mut self, target: u64) -> Result<u64> {
        if !self.based
            || self.segments >= self.replication.snapshot_segments
            || self.bytes >= self.replication.snapshot_bytes
        {
            return self.snapshot();
        }
        let mut uploaded = self.shared.uploaded();
        while uploaded < target {
            let entries = self.read(uploaded, target)?;
            // A missing entry means writes made without the log; only a snapshot carries them.
            if !entries.iter().zip(uploaded + 1..).all(|((sequence, _), expected)| *sequence == expected)
                || entries.is_empty()
            {
                return self.snapshot();
            }
            let last = entries.last().map_or(uploaded, |(sequence, _)| *sequence);
            let segment = segment::encode(self.epoch, entries.iter().map(|(_, entry)| entry.as_slice()))?;
            let size = segment.len() as u64;
            self.replication.storage.put(&segment::segment_key(self.epoch, uploaded + 1, last), segment)?;
            self.segments += 1;
            self.bytes += size;
            uploaded = last;
        }
        Ok(uploaded)
    }

    /// Reads entries after `after`, stopping at `through` or about one segment.
    fn read(&self, after: u64, through: u64) -> Result<Vec<(u64, Vec<u8>)>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT sequence, entry FROM _chunk_log WHERE sequence > ?1 AND sequence <= ?2 ORDER BY sequence",
        )?;
        let mut rows = statement.query([after, through])?;
        let (mut entries, mut bytes) = (Vec::new(), 0);
        while bytes < SEGMENT_BYTES {
            let Some(row) = rows.next()? else { break };
            let entry: Vec<u8> = row.get(1)?;
            bytes += entry.len();
            entries.push((row.get(0)?, entry));
        }
        Ok(entries)
    }

    fn snapshot(&mut self) -> Result<u64> {
        let copy = super::sibling(&self.path, "snapshot");
        let _ = fs::remove_file(&copy);
        let result = self.upload_snapshot(&copy);
        let _ = fs::remove_file(&copy);
        let sequence = result?;
        self.based = true;
        self.segments = 0;
        self.bytes = 0;
        Ok(sequence)
    }

    fn upload_snapshot(&self, copy: &Path) -> Result<u64> {
        let name = copy.to_str().ok_or(Error::Invalid("database path is not UTF-8"))?;
        super::create_private(copy)?;
        self.connection.execute("VACUUM INTO ?1", [name])?;
        let database = Connection::open(copy)?;
        database.pragma_update(None, "journal_mode", "DELETE")?;
        database.execute("DELETE FROM _chunk_log", [])?;
        let epoch = log::epoch(&database)?;
        let (sequence, _) = log::position(&database)?;
        database.close().map_err(|(_, error)| error)?;
        if epoch != self.epoch {
            return Err(Error::Corrupt("snapshot epoch changed"));
        }
        self.replication.storage.put(&segment::snapshot_key(self.epoch, sequence), fs::read(copy)?)?;
        Ok(sequence)
    }

    fn advance(&self, sequence: u64) {
        let mut state = self.shared.lock();
        state.uploaded = state.uploaded.max(sequence);
        state.pending_bytes = 0;
        if state.uploaded >= state.committed {
            state.pending_since = None;
            state.flush = false;
        } else {
            state.pending_since = Some(Instant::now());
        }
        self.shared.changed.notify_all();
    }
}

struct Exit<'a>(&'a Shared);

impl Drop for Exit<'_> {
    fn drop(&mut self) {
        self.0.lock().ended.get_or_insert(Ended::Stopped);
        self.0.changed.notify_all();
    }
}
