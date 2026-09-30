//! Shipping core's log lines and usage records to management: at least once, since management drops what it already
//! holds, and before core may be suspended or once it stops.

use super::usage::Usage;
use crate::logs::Lines;
use chunk_management::{Client, v1};
use std::{
    convert::Infallible,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, SystemTime},
};
use tokio::sync::Notify;

/// How often buffered lines and records are shipped.
const INTERVAL: Duration = Duration::from_secs(1);
/// The longest wait before a failed shipment is tried again.
const RETRY_LIMIT: Duration = Duration::from_secs(30);
/// How long one shipment may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// Management takes at most this many entries or records per call, and at most this much log text.
const MAX_BATCH: usize = 1000;
const MAX_TEXT_BYTES: usize = 512 * 1024;
/// Core may be suspended while its oldest unshipped line is younger than this, so a steady trickle of lines never
/// holds it awake; those lines ship once it resumes.
const SETTLE_LAG: Duration = Duration::from_secs(3);

pub(crate) struct Telemetry {
    client: Client,
    instance_id: String,
    lines: &'static Lines,
    usage: Mutex<Usage>,
    /// Notified to ship at once.
    ship: Notify,
}

impl Telemetry {
    pub(super) fn new(client: Client, instance_id: String, lines: &'static Lines, started: SystemTime) -> Self {
        let usage = Mutex::new(Usage::new(instance_id.clone(), started));
        Self { client, instance_id, lines, usage, ship: Notify::new() }
    }

    pub(super) fn lines(&self) -> &'static Lines {
        self.lines
    }

    /// Counts awake time with `players` online.
    pub(super) fn tick(&self, players: u32) {
        self.usage().tick(SystemTime::now(), players);
    }

    /// Whether core may be suspended as far as telemetry goes: every usage record reached management, and no log line
    /// waited longer than [`SETTLE_LAG`]. The span being counted stays open across a suspend and ends at its last count
    /// before it.
    pub(super) fn settled(&self) -> bool {
        let settled = self.usage().settled() && self.lines.oldest_at().is_none_or(|at| at.elapsed() < SETTLE_LAG);
        if !settled {
            self.ship.notify_one();
        }
        settled
    }

    /// Ships lines and records every [`INTERVAL`], or at once when asked, backing off after a failure.
    pub(super) async fn keep_shipping(&self) -> Infallible {
        let mut failures = 0_u32;
        loop {
            let wait = INTERVAL.saturating_mul(2_u32.saturating_pow(failures)).min(RETRY_LIMIT);
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = self.ship.notified(), if failures == 0 => {}
            }
            match self.ship_all().await {
                Ok(()) => failures = 0,
                Err(error) => {
                    if failures == 0 {
                        tracing::warn!(%error, "logs and usage not shipped to management; retrying");
                    }
                    failures = failures.saturating_add(1);
                }
            }
        }
    }

    /// Ends the usage span and ships what is buffered, within `bound`.
    pub(crate) async fn finish(&self, bound: Duration) {
        self.usage().end(SystemTime::now());
        match tokio::time::timeout(bound, self.ship_all()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "logs and usage not shipped before stopping"),
            Err(_) => tracing::warn!("logs and usage not shipped before stopping in time"),
        }
    }

    /// Ships every buffered record, then every buffered line, one batch at a time.
    async fn ship_all(&self) -> Result<(), String> {
        loop {
            let records = self.usage().pending(MAX_BATCH);
            if records.is_empty() {
                break;
            }
            let request = v1::ReportUsageRequest { records };
            within(self.client.report_usage(&request)).await?;
            self.usage().delivered(&request.records);
        }
        // Lines logged while shipping ship next time, so shipping's own lines can't keep this loop going.
        let Some(until) = self.lines.newest() else { return Ok(()) };
        loop {
            let batch = self.lines.oldest(MAX_BATCH, MAX_TEXT_BYTES, self.instance_id.len());
            let Some(last) = batch.last().map(|line| line.sequence) else { return Ok(()) };
            let entries = batch.into_iter().map(|line| self.entry(line)).collect();
            within(self.client.report_logs(&v1::ReportLogsRequest { entries })).await?;
            self.lines.delivered(last);
            if last >= until {
                return Ok(());
            }
        }
    }

    fn entry(&self, line: crate::logs::Line) -> v1::LogEntry {
        v1::LogEntry {
            time: Some(line.time.into()),
            source: line.source.into(),
            severity: line.severity.into(),
            message: line.message,
            instance_id: self.instance_id.clone(),
            app_id: String::new(),
            deployment_id: line.deployment.to_string(),
            sequence: line.sequence,
        }
    }

    fn usage(&self) -> MutexGuard<'_, Usage> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

async fn within<T>(call: impl Future<Output = Result<T, chunk_management::Error>>) -> Result<T, String> {
    match tokio::time::timeout(TIMEOUT, call).await {
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(_) => Err(format!("management did not answer within {}s", TIMEOUT.as_secs())),
    }
}
