//! The environment's native system module writes its reserved tables through a priority lane
//! on the commit thread: each durable write takes queued system commits ahead of app commits.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};

use chunk_store::{DatabaseSchema, Epoch, Revision, Snapshot, Write, is_system_table};

use crate::{Backend, Error, Result, commit::Job};

/// Operation IDs of system commits start with this; app operation IDs may not.
pub(crate) const OPERATION_PREFIX: &str = "chunk/";

pub(crate) enum SystemJob {
    Open { schema: DatabaseSchema, reply: mpsc::SyncSender<Result<Snapshot>> },
    Commit { writes: Vec<Write>, reply: mpsc::SyncSender<Result<Revision>> },
}

/// System jobs waiting for the commit thread, which drains them before each durable write.
#[derive(Default)]
pub(crate) struct Lane {
    queue: Mutex<Queue>,
    epoch: OnceLock<Epoch>,
    stopped: AtomicBool,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<SystemJob>,
    /// Wakes an idle commit thread; `None` until it starts and after it stops.
    wake: Option<mpsc::SyncSender<Job>>,
}

impl Lane {
    pub fn start(&self, wake: mpsc::SyncSender<Job>, epoch: Epoch) {
        let _ = self.epoch.set(epoch);
        if let Ok(mut queue) = self.queue.lock() {
            queue.wake = Some(wake);
        }
    }

    fn submit(&self, job: SystemJob) -> Result<()> {
        let mut queue = self.queue.lock().map_err(|_| Error::Closed)?;
        let wake = queue.wake.as_ref().ok_or(Error::Closed)?;
        // A full queue already guarantees the commit thread drains the lane again.
        if let Err(mpsc::TrySendError::Disconnected(_)) = wake.try_send(Job::Wake) {
            return Err(Error::Closed);
        }
        queue.jobs.push_back(job);
        Ok(())
    }

    pub fn take(&self) -> Vec<SystemJob> {
        self.queue.lock().map(|mut queue| queue.jobs.drain(..).collect()).unwrap_or_default()
    }

    /// Marks later system writes as unable to commit.
    pub fn fail(&self) {
        self.stopped.store(true, Ordering::Release);
    }

    /// Refuses further jobs and fails queued ones as closed.
    pub fn close(&self) {
        self.fail();
        if let Ok(mut queue) = self.queue.lock() {
            queue.wake = None;
            queue.jobs.clear();
        }
    }
}

/// Writes the environment's system tables, ahead of queued app commits. Apps can neither
/// read nor write these tables. Holding a handle keeps the backend running.
#[derive(Clone)]
pub struct System {
    _backend: Backend,
    lane: Arc<Lane>,
}

impl System {
    pub(crate) fn new(backend: Backend, lane: Arc<Lane>) -> Self {
        Self { _backend: backend, lane }
    }

    /// The store's epoch, fixed while the backend runs.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.lane.epoch.get().copied().unwrap_or_default()
    }

    /// Whether the commit pipeline failed or stopped, so no system write can commit again.
    #[must_use]
    pub fn stopped(&self) -> bool {
        self.lane.stopped.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.lane.queue.lock().map_or(0, |queue| queue.jobs.len())
    }

    /// Additively installs `schema`'s system tables, then reads the store.
    /// Blocks until the commit thread replies.
    /// # Errors
    /// Rejects tables outside the system prefix and incompatible changes, and reports a
    /// stopped or failed pipeline.
    pub fn open(&self, schema: DatabaseSchema) -> Result<Snapshot> {
        if !schema.keys().all(|table| is_system_table(table)) {
            return Err(Error::Invalid("system schema declares an app table"));
        }
        let (reply, result) = mpsc::sync_channel(1);
        self.lane.submit(SystemJob::Open { schema, reply })?;
        result.recv().map_err(|_| Error::Closed)?
    }

    /// Commits `writes` as one commit in the next durable write, ahead of queued app commits,
    /// and returns its revision. Blocks until it is durable.
    /// # Errors
    /// Rejects writes outside system tables, reports rejected commits, and reports a
    /// stopped or failed pipeline, after which the outcome may be unknown.
    pub fn commit(&self, writes: Vec<Write>) -> Result<Revision> {
        if !writes.iter().all(|write| is_system_table(&write.key.table)) {
            return Err(Error::Invalid("system commit writes an app table"));
        }
        let (reply, result) = mpsc::sync_channel(1);
        self.lane.submit(SystemJob::Commit { writes, reply })?;
        result.recv().map_err(|_| Error::Closed)?
    }
}
