//! Work a process is doing, observed without missing work that came and went between two looks.

use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
};

/// Work in flight, and a count of every start and finish of work, which moves even when work began and ended between
/// two observations.
#[derive(Clone, Debug, Default)]
pub struct Activity(Arc<Counters>);

#[derive(Debug, Default)]
struct Counters {
    changes: AtomicU64,
    in_flight: AtomicUsize,
}

/// What [`Activity::observe`] saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    /// Moves with every start and finish of work.
    pub changes: u64,
    /// Work begun and not finished.
    pub in_flight: usize,
}

impl Activity {
    /// Records work that began and ended at once.
    pub fn touch(&self) {
        self.0.changes.fetch_add(1, Ordering::SeqCst);
    }

    /// Records work that runs until the returned guard drops.
    #[must_use]
    pub fn begin(&self) -> Busy {
        self.0.in_flight.fetch_add(1, Ordering::SeqCst);
        self.touch();
        Busy(self.clone())
    }

    #[must_use]
    pub fn observe(&self) -> Observed {
        Observed { changes: self.0.changes.load(Ordering::SeqCst), in_flight: self.0.in_flight.load(Ordering::SeqCst) }
    }
}

/// Work in flight, which finishes when this drops.
#[derive(Debug)]
pub struct Busy(Activity);

impl Drop for Busy {
    fn drop(&mut self) {
        self.0.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.touch();
    }
}
