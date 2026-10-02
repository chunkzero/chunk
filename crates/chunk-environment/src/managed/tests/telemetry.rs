//! Log and usage shipments against the fake management.

use super::{launcher::respond, *};
use crate::{
    logs::Lines,
    managed::{Lease, telemetry::Telemetry},
};
use chunk_management::v1::{LogEntry, LogSeverity, LogSource, ReportLogsRequest, ReportUsageRequest, UsageRecord};
use std::time::SystemTime;

/// What management holds of the environment's reports, deduplicated by the rules of `packages/management`.
#[derive(Default)]
pub(super) struct Reports {
    /// Every `ReportLogs` batch, in order.
    batches: Vec<Vec<LogEntry>>,
    /// Log entries by instance and sequence.
    logs: BTreeMap<(String, u64), LogEntry>,
    /// Usage records by ID.
    usage: BTreeMap<String, UsageRecord>,
    /// How many more `ReportLogs` replies are lost once management stored their entries.
    lost: usize,
}

/// Answers a `ReportUsage`, `ReportLogs`, `ReportMetrics` or `ReportFailedAuth` call.
pub(super) fn serve(management: &Management, path: &str, body: &[u8]) -> hyper::Response<Body> {
    let mut reports = management.telemetry.lock().unwrap();
    if path.ends_with("/ReportUsage") {
        for record in ReportUsageRequest::decode(body).unwrap().records {
            reports.usage.entry(record.id.clone()).or_insert(record);
        }
    } else if path.ends_with("/ReportLogs") {
        let entries = ReportLogsRequest::decode(body).unwrap().entries;
        for entry in &entries {
            reports.logs.entry((entry.instance_id.clone(), entry.sequence)).or_insert_with(|| entry.clone());
        }
        reports.batches.push(entries);
        if reports.lost > 0 {
            reports.lost -= 1;
            return respond(503, "application/json", UNAVAILABLE.into());
        }
    }
    respond(200, "application/proto", Vec::new())
}

#[tokio::test]
async fn lines_are_shipped_again_until_acknowledged_and_the_oldest_drop_once_the_buffer_is_full() {
    let harness = Harness::new().await;
    let lines: &'static Lines = Box::leak(Box::new(Lines::new()));
    let client = harness.management_config().client();
    let (_lease, held) = watch::channel(Lease::Held(1));
    let telemetry = Telemetry::new(client, "core-1".into(), lines, held);
    telemetry.tick(0);
    for line in ["first", "second"] {
        lines.push(LogSource::Core, LogSeverity::Info, line.into(), None);
    }
    lines.serving("dep_a");
    lines.push(LogSource::Gateway, LogSeverity::Warn, "third".into(), None);

    // Management stores the batch but its reply is lost, so the lines stay until the next shipment dedupes them.
    harness.management.telemetry.lock().unwrap().lost = 1;
    telemetry.finish(Duration::from_secs(5)).await;
    assert_eq!(lines.newest(), Some(3));
    assert!(lines.oldest_at().is_some());
    telemetry.finish(Duration::from_secs(5)).await;
    assert_eq!(lines.oldest_at(), None);
    {
        let reports = harness.management.telemetry.lock().unwrap();
        assert_eq!(reports.batches.len(), 2);
        let stored: Vec<_> = reports
            .logs
            .values()
            .map(|entry| (entry.sequence, entry.message.as_str(), entry.deployment_id.as_str(), entry.source()))
            .collect();
        assert_eq!(
            stored,
            [
                (1, "first", "", LogSource::Core),
                (2, "second", "", LogSource::Core),
                (3, "third", "dep_a", LogSource::Gateway)
            ]
        );
        assert!(reports.logs.keys().all(|(instance, _)| instance == "core-1"));
        // The usage span ended with the first stop and was recorded once.
        assert!(!reports.usage.is_empty() && reports.usage.keys().all(|id| id.starts_with("core-1/")));
    }

    // More than the buffer holds, each cut to 8 KiB: the oldest go, and what remains ships in batches management
    // accepts.
    let long = "x".repeat(64 * 1024);
    for _ in 0..1_000 {
        lines.push(LogSource::Core, LogSeverity::Info, long.clone(), None);
    }
    let dropped = lines.dropped();
    assert!(dropped > 0);
    telemetry.finish(Duration::from_secs(5)).await;
    let reports = harness.management.telemetry.lock().unwrap();
    let later = &reports.batches[2..];
    assert!(later.len() > 1);
    for batch in later {
        let text: usize = batch.iter().map(|entry| entry.message.len() + entry.instance_id.len()).sum();
        assert!(batch.len() <= 1000 && text <= 512 * 1024);
        assert!(batch.iter().all(|entry| entry.message.len() == 8 * 1024));
    }
    let sequences: Vec<_> = later.iter().flatten().map(|entry| entry.sequence).collect();
    let kept = 1_000 - dropped;
    assert_eq!(sequences, (1_004 - kept..1_004).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn a_slow_shutdown_is_counted_as_awake() {
    let harness = Harness::new().await;
    let lines: &'static Lines = Box::leak(Box::new(Lines::new()));
    let (_lease, held) = watch::channel(Lease::Held(1));
    let (wall, paused) = (SystemTime::now(), tokio::time::Instant::now());
    let telemetry = Telemetry::new(harness.management_config().client(), "core-1".into(), lines, held)
        .with_clock(move || wall + paused.elapsed());
    telemetry.tick(1);
    // Twice as long as a gap that would read as a suspend.
    telemetry.counting(tokio::time::sleep(Duration::from_secs(20))).await;
    tokio::time::resume();
    telemetry.finish(Duration::from_secs(5)).await;

    let reports = harness.management.telemetry.lock().unwrap();
    let spans: Vec<_> = reports
        .usage
        .values()
        .map(|record| {
            let [start, end] =
                [record.start_time, record.end_time].map(|time| SystemTime::try_from(time.unwrap()).unwrap());
            end.duration_since(start).unwrap().as_secs()
        })
        .collect();
    assert_eq!(spans, [20]);
}
