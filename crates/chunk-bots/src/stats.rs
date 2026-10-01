//! Counters and latency histograms shared by every bot, with the progress line and final summary built from them.
use std::{
    collections::BTreeMap,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::Duration,
};

use hdrhistogram::Histogram;
use serde::Serialize;

pub struct Stats {
    pub started: AtomicU64,
    pub logged_in: AtomicU64,
    /// Bots that reached play at least once.
    pub spawned: AtomicU64,
    /// Bots in play now, not counting those between sessions.
    pub playing: AtomicU64,
    /// Bots that ended before reaching play.
    pub failed: AtomicU64,
    /// Bots that reached play and were then disconnected.
    pub disconnects: AtomicU64,
    /// Returns to configuration, as a move between sessions does.
    pub reconfigurations: AtomicU64,
    pub pings: AtomicU64,
    pub pongs: AtomicU64,
    /// Pings unanswered for over thirty seconds, which no pong is counted for.
    pub ping_timeouts: AtomicU64,
    pub commands: AtomicU64,
    pub bytes_in: AtomicU64,
    /// Microseconds from connecting to the first spawn in play.
    login: Mutex<Histogram<u64>>,
    /// Play-state ping round trips in microseconds, over the run and since the last progress line.
    rtt: Mutex<[Histogram<u64>; 2]>,
    /// Why bots failed or disconnected, with counts.
    reasons: Mutex<BTreeMap<String, u64>>,
}

#[derive(Debug, Serialize)]
pub struct Percentiles {
    pub count: u64,
    pub p50: f64,
    pub p90: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
}

impl Percentiles {
    /// Milliseconds from a histogram of microseconds.
    fn of(histogram: &Histogram<u64>) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let ms = |us: u64| (us as f64 / 100.0).round() / 10.0;
        let at = |quantile| ms(histogram.value_at_quantile(quantile));
        Self {
            count: histogram.len(),
            p50: at(0.5),
            p90: at(0.9),
            p99: at(0.99),
            p999: at(0.999),
            max: ms(histogram.max()),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct Summary {
    pub elapsed_seconds: f64,
    pub started: u64,
    pub logged_in: u64,
    pub spawned: u64,
    pub playing: u64,
    pub failed: u64,
    pub disconnects: u64,
    pub reconfigurations: u64,
    pub pings: u64,
    pub pongs: u64,
    pub ping_timeouts: u64,
    pub commands: u64,
    pub bytes_in: u64,
    pub login_to_play_ms: Percentiles,
    pub ping_rtt_ms: Percentiles,
    pub reasons: BTreeMap<String, u64>,
    pub cpu_seconds: f64,
    pub cpu_cores: f64,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Up to an hour in microseconds; longer values saturate.
fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 3_600_000_000, 3).expect("valid histogram bounds")
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

impl Stats {
    pub fn new() -> Self {
        Self {
            started: AtomicU64::new(0),
            logged_in: AtomicU64::new(0),
            spawned: AtomicU64::new(0),
            playing: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            disconnects: AtomicU64::new(0),
            reconfigurations: AtomicU64::new(0),
            pings: AtomicU64::new(0),
            pongs: AtomicU64::new(0),
            ping_timeouts: AtomicU64::new(0),
            commands: AtomicU64::new(0),
            bytes_in: AtomicU64::new(0),
            login: Mutex::new(histogram()),
            rtt: Mutex::new([histogram(), histogram()]),
            reasons: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn spawned(&self, login: Duration) {
        self.spawned.fetch_add(1, Relaxed);
        lock(&self.login).saturating_record(micros(login));
    }

    pub fn entered_play(&self) {
        self.playing.fetch_add(1, Relaxed);
    }

    pub fn left_play(&self) {
        self.playing.fetch_sub(1, Relaxed);
    }

    pub fn pong(&self, rtt: Duration) {
        self.pongs.fetch_add(1, Relaxed);
        for histogram in lock(&self.rtt).iter_mut() {
            histogram.saturating_record(micros(rtt));
        }
    }

    /// Records how a bot ended: `reason` is None when the run stopped it.
    pub fn ended(&self, spawned: bool, reason: Option<String>) {
        let Some(reason) = reason else { return };
        let counter = if spawned { &self.disconnects } else { &self.failed };
        if counter.fetch_add(1, Relaxed) < 5 {
            eprintln!("bot {}: {reason}", if spawned { "disconnected" } else { "failed" });
        }
        *lock(&self.reasons).entry(reason).or_default() += 1;
    }

    /// One progress line, resetting the recent round-trip window.
    pub fn progress(&self, elapsed: Duration, bytes_per_second: f64, cpu_cores: f64) -> String {
        let login = Percentiles::of(&lock(&self.login));
        let recent = {
            let mut rtt = lock(&self.rtt);
            let recent = Percentiles::of(&rtt[1]);
            rtt[1].reset();
            recent
        };
        format!(
            "{:>6.0}s started {} playing {} failed {} disconnected {} | login p50 {} p99 {} ms | rtt p50 {} p99 {} ms \
             ({} pongs) | in {:.2} MB/s | cpu {cpu_cores:.2}",
            elapsed.as_secs_f64(),
            self.started.load(Relaxed),
            self.playing.load(Relaxed),
            self.failed.load(Relaxed),
            self.disconnects.load(Relaxed),
            login.p50,
            login.p99,
            recent.p50,
            recent.p99,
            recent.count,
            bytes_per_second / 1e6,
        )
    }

    pub fn summary(&self, elapsed: Duration, cpu_seconds: f64) -> Summary {
        let elapsed_seconds = elapsed.as_secs_f64();
        Summary {
            elapsed_seconds: (elapsed_seconds * 10.0).round() / 10.0,
            started: self.started.load(Relaxed),
            logged_in: self.logged_in.load(Relaxed),
            spawned: self.spawned.load(Relaxed),
            playing: self.playing.load(Relaxed),
            failed: self.failed.load(Relaxed),
            disconnects: self.disconnects.load(Relaxed),
            reconfigurations: self.reconfigurations.load(Relaxed),
            pings: self.pings.load(Relaxed),
            pongs: self.pongs.load(Relaxed),
            ping_timeouts: self.ping_timeouts.load(Relaxed),
            commands: self.commands.load(Relaxed),
            bytes_in: self.bytes_in.load(Relaxed),
            login_to_play_ms: Percentiles::of(&lock(&self.login)),
            ping_rtt_ms: Percentiles::of(&lock(&self.rtt)[0]),
            reasons: lock(&self.reasons).clone(),
            cpu_seconds: (cpu_seconds * 100.0).round() / 100.0,
            cpu_cores: (cpu_seconds / elapsed_seconds.max(0.001) * 1000.0).round() / 1000.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_counts_outcomes_and_reports_latency_in_milliseconds() {
        let stats = Stats::new();
        for ms in 1..=100 {
            stats.spawned(Duration::from_millis(ms * 10));
            stats.entered_play();
            stats.pong(Duration::from_millis(ms));
        }
        stats.left_play();
        stats.left_play();
        stats.ended(true, None);
        stats.ended(true, Some("kicked: Timed out".into()));
        stats.ended(false, Some("connection refused".into()));
        stats.ended(false, Some("connection refused".into()));
        let summary = stats.summary(Duration::from_secs(10), 2.5);
        assert_eq!((summary.spawned, summary.playing, summary.disconnects, summary.failed), (100, 98, 1, 2));
        assert_eq!(summary.reasons["connection refused"], 2);
        assert_eq!((summary.ping_rtt_ms.count, summary.ping_rtt_ms.p50, summary.ping_rtt_ms.max), (100, 50.0, 100.0));
        // Three significant figures: within 0.1% of the recorded value.
        assert!((summary.login_to_play_ms.p99 - 990.0).abs() <= 1.0);
        assert!((summary.cpu_cores - 0.25).abs() < 1e-9);
        // The progress line empties the recent window but not the run's histogram.
        stats.progress(Duration::from_secs(10), 0.0, 0.0);
        assert_eq!(lock(&stats.rtt)[1].len(), 0);
        assert_eq!(stats.summary(Duration::from_secs(10), 0.0).ping_rtt_ms.count, 100);
    }
}
