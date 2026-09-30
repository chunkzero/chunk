//! A few gauges and counters for operating the environment, reported to management best effort.

use super::Managed;
use chunk_management::v1;
use chunk_proto::sync::v1::NodePhase;
use std::{
    collections::HashMap,
    convert::Infallible,
    time::{Duration, SystemTime},
};
use tokio::time::MissedTickBehavior;

/// How often metrics are reported.
const INTERVAL: Duration = Duration::from_secs(10);
/// How long one report may take; a lost one isn't sent again.
const TIMEOUT: Duration = Duration::from_secs(5);

impl Managed<'_> {
    /// Reports [`Self::samples`] every [`INTERVAL`].
    pub(super) async fn report_metrics(&self) -> Infallible {
        let mut tick = tokio::time::interval(INTERVAL);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            let request = v1::ReportMetricsRequest { samples: self.samples() };
            match tokio::time::timeout(TIMEOUT, self.client.report_metrics(&request)).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => tracing::debug!(%error, "metrics not reported"),
                Err(_) => tracing::debug!("metrics not reported in time"),
            }
        }
    }

    /// Players, sessions and JVMs from control and their JVMs' health reports, connections gateways report, backend
    /// work in flight, and this process's CPU time, memory and dropped log lines. What can't be read is left out.
    fn samples(&self) -> Vec<v1::MetricSample> {
        let mut values = Vec::new();
        if let Ok(control) = self.core.control() {
            if let Ok(online) = control.online_players() {
                values.push(("online_players", gauge(online as u64)));
            }
            values.push(("logins_total", gauge(control.logins())));
            if let Ok(nodes) = control.nodes() {
                let live: Vec<_> = nodes.iter().filter(|node| node.phase != NodePhase::Stopped).collect();
                let health = || live.iter().filter_map(|node| node.health.as_ref());
                values.extend([
                    ("jvm_hosts", gauge(live.len() as u64)),
                    ("sessions", gauge(health().map(|health| u64::from(health.sessions)).sum())),
                    ("jvm_heap_used_bytes", gauge(health().map(|health| health.heap_used_bytes).sum())),
                    (
                        "jvm_tick_age_max_millis",
                        gauge(health().map(|health| health.last_tick_age_millis).max().unwrap_or(0)),
                    ),
                ]);
            }
        }
        values.push(("gateway_connections", gauge(self.core.gateway_connections())));
        if let Some(backend) = self.core.backend() {
            values.push(("backend_work_in_flight", gauge(backend.activity().observe().in_flight as u64)));
        }
        #[cfg(unix)]
        values.push(("process_cpu_seconds_total", {
            let cpu = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
            Duration::new(cpu.tv_sec.unsigned_abs(), u32::try_from(cpu.tv_nsec).unwrap_or(0)).as_secs_f64()
        }));
        if let Some(resident) = resident_bytes() {
            values.push(("process_resident_memory_bytes", gauge(resident)));
        }
        values.push(("log_lines_dropped_total", gauge(self.telemetry.lines().dropped())));
        let time = Some(SystemTime::now().into());
        let sample = |(name, value): (&str, f64)| v1::MetricSample {
            name: name.into(),
            labels: HashMap::default(),
            time,
            value,
            instance_id: self.instance_id.clone(),
        };
        values.into_iter().map(sample).collect()
    }
}

#[allow(clippy::cast_precision_loss)]
fn gauge(value: u64) -> f64 {
    value as f64
}

/// This process's resident memory, from `/proc/self/status`.
fn resident_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|line| line.strip_prefix("VmRSS:"))?;
    let kib: u64 = line.trim().strip_suffix("kB")?.trim().parse().ok()?;
    Some(kib * 1024)
}
