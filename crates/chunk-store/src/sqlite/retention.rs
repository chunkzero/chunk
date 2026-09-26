use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;

use crate::Result;

/// Rows removed per table and pass, so pruning never stalls a commit for long.
const PRUNE_ROWS: i64 = 10_000;

/// How long records stay after they stop changing. Expired records are deleted
/// in ordinary write transactions, so replicas and restores forget them too.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Retention {
    /// Committed operation outcomes. Retrying an operation with an unknown
    /// outcome inside this window returns its outcome; later it runs again.
    pub outcomes: Duration,
    /// Fixed invocation inputs of operations that never committed.
    pub retry_contexts: Duration,
    /// Succeeded, failed, unknown and cancelled job records.
    pub jobs: Duration,
}

impl Default for Retention {
    fn default() -> Self {
        let day = Duration::from_hours(24);
        Self { outcomes: day, retry_contexts: day, jobs: day }
    }
}

/// Milliseconds since the Unix epoch.
pub(super) fn now() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis()).unwrap_or(i64::MAX)
}

fn cutoff(now: i64, window: Duration) -> i64 {
    now.saturating_sub(i64::try_from(window.as_millis()).unwrap_or(i64::MAX))
}

pub(super) fn prune_operations(connection: &Connection, retention: &Retention, now: i64) -> Result<()> {
    connection.execute(
        "DELETE FROM _chunk_operations WHERE operation_id IN (
            SELECT operation_id FROM _chunk_operations WHERE committed_at < ?1 ORDER BY committed_at LIMIT ?2)",
        [cutoff(now, retention.outcomes), PRUNE_ROWS],
    )?;
    connection.execute(
        "DELETE FROM _chunk_retry_contexts WHERE operation_id IN (
            SELECT operation_id FROM _chunk_retry_contexts WHERE prepared_at < ?1 LIMIT ?2)",
        [cutoff(now, retention.retry_contexts), PRUNE_ROWS],
    )?;
    Ok(())
}

/// Reports whether any finished job record expired.
pub(super) fn prune_jobs(connection: &Connection, retention: &Retention, now: i64) -> Result<bool> {
    let removed = connection.execute(
        "DELETE FROM _chunk_jobs WHERE state NOT IN ('pending', 'running') AND updated_at < ?1",
        [cutoff(now, retention.jobs)],
    )?;
    Ok(removed > 0)
}
