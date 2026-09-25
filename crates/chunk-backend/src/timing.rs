//! Opt-in phase timings for local benchmarks. Without the `bench-support` feature,
//! timers are empty and record nothing.
#[cfg(feature = "bench-support")]
use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

/// A stage of request handling on the engine or commit thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// A query or mutation request was admitted until the engine thread took it.
    Queue,
    /// Query evaluation, including result validation.
    Query,
    /// Retry-context persistence on the commit thread before a mutation executes.
    Prepare,
    /// Mutation evaluation and write validation before staging.
    Mutation,
    /// Durable commit and snapshot refresh on the commit thread.
    Commit,
    /// A mutation was staged until the engine thread handled its durable acknowledgment.
    Durable,
    /// Reevaluation of one affected subscription group.
    Reevaluate,
    /// A reevaluation batch was queued until every subscription in it was checked and
    /// published. A coalesced batch reports only its latest commit.
    FanOut,
}

#[cfg(feature = "bench-support")]
static OBSERVER: OnceLock<fn(Phase, Duration)> = OnceLock::new();

/// Installs the process-wide observer. Returns false if one was already installed.
#[cfg(feature = "bench-support")]
pub fn observe(observer: fn(Phase, Duration)) -> bool {
    OBSERVER.set(observer).is_ok()
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Timer {
    #[cfg(feature = "bench-support")]
    start: Instant,
}

impl Timer {
    pub(crate) fn start() -> Self {
        Self {
            #[cfg(feature = "bench-support")]
            start: Instant::now(),
        }
    }

    #[cfg_attr(not(feature = "bench-support"), allow(clippy::unused_self, unused_variables))]
    pub(crate) fn stop(self, phase: Phase) {
        #[cfg(feature = "bench-support")]
        if let Some(observer) = OBSERVER.get() {
            observer(phase, self.start.elapsed());
        }
    }
}
