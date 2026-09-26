use std::{collections::BTreeMap, fs::File, path::Path, time::Duration};

use anyhow::Result;
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use serde_json::{Value, json};

pub struct Stats {
    pub offered: u64,
    pub completed: u64,
    pub dropped_busy: u64,
    pub dropped_late: u64,
    pub errors: BTreeMap<String, u64>,
    latency: Histogram<u64>,
    success_latency: Histogram<u64>,
    scheduler_lag: Histogram<u64>,
    pub elapsed: Duration,
}

impl Stats {
    pub fn new(offered: u64) -> Result<Self> {
        Ok(Self {
            offered,
            completed: 0,
            dropped_busy: 0,
            dropped_late: 0,
            errors: BTreeMap::new(),
            latency: histogram()?,
            success_latency: histogram()?,
            scheduler_lag: histogram()?,
            elapsed: Duration::ZERO,
        })
    }

    pub fn scheduled(&mut self, lag: Duration) -> Result<()> {
        self.scheduler_lag.record(micros(lag))?;
        Ok(())
    }

    pub fn observe(&mut self, latency: Duration, error: Option<String>) -> Result<()> {
        self.latency.record(micros(latency))?;
        if let Some(error) = error {
            *self.errors.entry(error).or_default() += 1;
        } else {
            self.completed += 1;
            self.success_latency.record(micros(latency))?;
        }
        Ok(())
    }

    pub fn summary(&self, seconds: u32, bytes_per_operation: usize) -> Value {
        let completed = count(self.completed);
        let failed: u64 = self.errors.values().sum();
        json!({
            "offered": self.offered, "completed": self.completed, "failed": failed,
            "dropped_busy": self.dropped_busy, "dropped_late": self.dropped_late, "errors": self.errors,
            "accounted": self.offered == self.completed + failed + self.dropped_busy + self.dropped_late,
            "offered_window_seconds": seconds, "elapsed_including_drain_seconds": self.elapsed.as_secs_f64(),
            "completed_per_offered_second": completed / f64::from(seconds),
            "completed_per_elapsed_second": completed / self.elapsed.as_secs_f64(),
            "delivered_application_mib_per_elapsed_second": completed * count(bytes_per_operation as u64) / 1_048_576.0 / self.elapsed.as_secs_f64(),
            "latency_us": distribution(&self.latency), "successful_latency_us": distribution(&self.success_latency),
            "scheduler_lag_us": distribution(&self.scheduler_lag)
        })
    }

    pub fn write(&self, directory: &Path, prefix: &str) -> Result<()> {
        for (name, histogram) in
            [("latency", &self.latency), ("success", &self.success_latency), ("scheduler", &self.scheduler_lag)]
        {
            V2Serializer::new()
                .serialize(histogram, &mut File::create(directory.join(format!("{prefix}-{name}.hdr")))?)?;
        }
        Ok(())
    }
}

pub fn save(histogram: &Histogram<u64>, path: &Path) -> Result<()> {
    V2Serializer::new().serialize(histogram, &mut File::create(path)?)?;
    Ok(())
}

pub fn histogram() -> Result<Histogram<u64>> {
    // Fixed storage, microseconds, 3 significant digits, up to two hours.
    Ok(Histogram::new_with_bounds(1, 7_200_000_000, 3)?)
}

/// Target phase timings: nanoseconds, 3 significant digits, up to one hour.
pub fn nanosecond_histogram() -> Result<Histogram<u64>> {
    Ok(Histogram::new_with_bounds(1, 3_600_000_000_000, 3)?)
}

pub fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX).max(1)
}

pub fn distribution(histogram: &Histogram<u64>) -> Value {
    if histogram.is_empty() {
        return Value::Null;
    }
    json!({"samples": histogram.len(), "p50": histogram.value_at_quantile(0.5),
        "p95": histogram.value_at_quantile(0.95), "p99": histogram.value_at_quantile(0.99),
        "max": histogram.max(), "mean": histogram.mean()})
}

/// Summarizes a nanosecond histogram in fractional microseconds.
pub fn nanosecond_distribution(histogram: &Histogram<u64>) -> Value {
    let us = |nanos: u64| count(nanos) / 1000.0;
    json!({"samples": histogram.len(), "p50": us(histogram.value_at_quantile(0.5)),
        "p95": us(histogram.value_at_quantile(0.95)), "p99": us(histogram.value_at_quantile(0.99)),
        "max": us(histogram.max()), "mean": histogram.mean() / 1000.0})
}

#[allow(clippy::cast_precision_loss)]
pub fn count(value: u64) -> f64 {
    value as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_and_drops_do_not_disappear_from_the_denominator() {
        let mut stats = Stats::new(4).unwrap();
        stats.observe(Duration::from_millis(2), None).unwrap();
        stats.observe(Duration::from_secs(1), Some("timeout".into())).unwrap();
        stats.dropped_busy = 1;
        stats.dropped_late = 1;
        stats.elapsed = Duration::from_secs(2);
        let summary = stats.summary(1, 1024);
        assert_eq!(summary["accounted"], true);
        assert_eq!(summary["completed_per_elapsed_second"], 0.5);
        assert_eq!(summary["latency_us"]["samples"], 2);
        assert_eq!(summary["successful_latency_us"]["samples"], 1);
        assert!(summary["latency_us"]["p99"].as_u64().unwrap() >= 1_000_000);
    }
}
