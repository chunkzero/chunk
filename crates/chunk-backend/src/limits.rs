//! Admission limits. New work is refused once queued work of its kind waits too long
//! or a memory budget runs out, and each refusal names its limit.
use std::{
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use crate::{Error, Result};

/// Longest wait queued work may reach before new work of the same kind is refused.
pub(crate) const QUEUE_WAIT: Duration = Duration::from_millis(500);
/// Admitted requests, including replies waiting for durability, charge their input plus this.
pub(crate) const REQUEST_OVERHEAD: usize = 1024;
pub(crate) const REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// Subscribed calls and their latest results.
pub(crate) const SUBSCRIPTION_BYTES: usize = 256 * 1024 * 1024;
/// Admitted mutations plus staged writes, results and replaced values.
pub(crate) const MUTATION_BYTES: usize = 32 * 1024 * 1024;
/// Each live action reserves its engine's heap limit.
pub(crate) const ACTION_BYTES: usize = 256 * 1024 * 1024;
/// Retained action outcomes, live actions and prepared action identities; outcomes give way first.
pub(crate) const RETAINED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Limit {
    #[error("requests waited over 500 ms for the engine thread")]
    EngineQueue,
    #[error("queries waited over 500 ms for a read engine")]
    ReadQueue,
    #[error("mutations waited over 500 ms to commit")]
    CommitQueue,
    #[error("admitted requests reached 64 MiB")]
    RequestMemory,
    #[error("pending mutations reached 32 MiB")]
    MutationMemory,
    #[error("subscriptions reached 256 MiB")]
    SubscriptionMemory,
    #[error("live actions reached 256 MiB of engine heap")]
    ActionMemory,
    #[error("live actions and prepared action identities reached 64 MiB of retention")]
    Retention,
    #[error("scheduled jobs reached the store's job budget; retry once jobs finish or expire")]
    Jobs,
}

impl Limit {
    pub(crate) fn exceeded(self) -> Error {
        Error::Overloaded(self)
    }
}

/// Wait for the engine thread, shared by request admission and the engine.
#[derive(Default)]
pub(crate) struct EngineQueue {
    queued: AtomicUsize,
    /// Nanoseconds the most recently dequeued request waited.
    wait: AtomicU64,
}

impl EngineQueue {
    pub fn enter(&self) -> Result<()> {
        let waited = Duration::from_nanos(self.wait.load(Ordering::Relaxed));
        if self.queued.load(Ordering::Relaxed) != 0 && waited > QUEUE_WAIT {
            return Err(Limit::EngineQueue.exceeded());
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Undoes `enter` for a request that never reached the queue.
    pub fn leave(&self) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn dequeued(&self, admitted: Instant) {
        let waited = u64::try_from(admitted.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.wait.store(waited, Ordering::Relaxed);
        self.leave();
    }
}
