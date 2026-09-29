//! When core may be suspended: after a grace period without activity under one desired revision.

use std::{
    sync::{Mutex, PoisonError},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

pub(super) struct Idle {
    /// How long core stays idle before it may be suspended; unset, it never may.
    grace: Option<Duration>,
    /// Since when nothing was active under the latest observation's desired revision.
    since: Mutex<Option<Since>>,
}

struct Since {
    revision: u64,
    /// The activity count then.
    changes: u64,
    at: Instant,
}

/// What core observed that decides whether it may be suspended.
pub(super) struct Observation {
    /// Work is running, players are online or on their way, a host is launching or a deployment is unfinished.
    pub active: bool,
    /// Moves whenever work starts or finishes, so work that came and went since the last observation is seen.
    pub changes: u64,
    /// The log is flushed and the wake alarm handed off.
    pub settled: bool,
    /// When the next job is due, in Unix milliseconds.
    pub due_at: Option<i64>,
}

impl Idle {
    pub(super) fn new(grace: Option<Duration>) -> Self {
        Self { grace, since: Mutex::default() }
    }

    /// Whether core ever may be suspended.
    pub(super) fn sleeps(&self) -> bool {
        self.grace.is_some()
    }

    /// Records `observed` under desired `revision` and returns whether core may be suspended: nothing was active for
    /// the grace period under this revision, the observation is settled, and no job is due within the grace period.
    /// Activity, including work that started or finished since the last observation, or a newer revision such as a
    /// wake brings, starts the grace period over. An observation under an older revision is never ready.
    pub(super) fn ready(&self, revision: u64, observed: &Observation) -> bool {
        let Some(grace) = self.grace else { return false };
        let now = Instant::now();
        let mut since = self.since.lock().unwrap_or_else(PoisonError::into_inner);
        let idle = match &*since {
            Some(latest) if revision < latest.revision => return false,
            Some(latest) if revision == latest.revision && !observed.active && observed.changes == latest.changes => {
                latest.at
            }
            _ => {
                *since = Some(Since { revision, changes: observed.changes, at: now });
                now
            }
        };
        let horizon = SystemTime::now() + grace;
        let due = observed.due_at.is_some_and(|due_at| {
            let due_at = UNIX_EPOCH + Duration::from_millis(u64::try_from(due_at).unwrap_or(0));
            due_at <= horizon
        });
        !observed.active && observed.settled && !due && now.duration_since(idle) >= grace
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn activity_a_new_revision_and_a_due_job_hold_core_awake() {
        let idle = Idle::new(Some(Duration::from_secs(60)));
        let quiet = Observation { active: false, changes: 0, settled: true, due_at: None };
        assert!(!idle.ready(1, &quiet));
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!idle.ready(1, &quiet));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(idle.ready(1, &quiet));
        assert!(!idle.ready(1, &Observation { settled: false, ..quiet }));
        assert!(idle.ready(1, &quiet));

        // A job due within the grace period holds core awake without starting it over.
        let soon = SystemTime::now() + Duration::from_secs(30);
        let due_at = i64::try_from(soon.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
        assert!(!idle.ready(1, &Observation { due_at: Some(due_at), ..quiet }));
        assert!(idle.ready(1, &Observation { due_at: Some(due_at + 3_600_000), ..quiet }));

        assert!(!idle.ready(1, &Observation { active: true, ..quiet }));
        assert!(!idle.ready(1, &quiet));
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(idle.ready(1, &quiet));

        // Work that started and finished between two observations starts it over too.
        let after_burst = Observation { changes: 2, ..quiet };
        assert!(!idle.ready(1, &after_burst));
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!idle.ready(1, &after_burst));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(idle.ready(1, &after_burst));
        let quiet = after_burst;

        // A wake's revision starts the grace period over, and an older revision's observation is never ready.
        assert!(!idle.ready(2, &quiet));
        tokio::time::advance(Duration::from_secs(60)).await;
        assert!(!idle.ready(1, &quiet));
        assert!(idle.ready(2, &quiet));

        assert!(!Idle::new(None).ready(1, &quiet));
    }
}
