//! Awake time and player time, recorded as spans for management's accounting.

use chunk_management::v1;
use std::{
    collections::VecDeque,
    time::{Duration, SystemTime},
};

/// How often awake and player time are counted.
pub(super) const TICK: Duration = Duration::from_secs(1);
/// A longer wait between two counts means the process was suspended or frozen meanwhile, which isn't awake time.
const GAP: Duration = Duration::from_secs(10);
/// How long a span runs before it is recorded, so a crash loses at most this much.
const SPAN: Duration = Duration::from_secs(60);
/// Records kept for management at most; the oldest go first.
const MAX_PENDING: usize = 10_000;

/// The span being counted, and the records management hasn't taken yet.
pub(super) struct Usage {
    /// Unique to this run of the process, so record IDs never repeat across runs.
    instance_id: String,
    /// Numbers the next record.
    index: u64,
    start: SystemTime,
    /// The latest count, or the start before the first.
    last: SystemTime,
    ticked: bool,
    player_millis: u128,
    pending: VecDeque<v1::UsageRecord>,
}

impl Usage {
    pub(super) fn new(instance_id: String, start: SystemTime) -> Self {
        Self { instance_id, index: 0, start, last: start, ticked: false, player_millis: 0, pending: VecDeque::new() }
    }

    /// Counts the time since the last count, with `players` online, as awake. After a longer gap than [`GAP`], or when
    /// the clock went back, the span ends at the last count and a new one starts `now`. A span [`SPAN`] long is
    /// recorded.
    pub(super) fn tick(&mut self, now: SystemTime, players: u32) {
        match now.duration_since(self.last) {
            Ok(elapsed) if elapsed <= GAP || !self.ticked => {
                self.player_millis += u128::from(players) * elapsed.as_millis();
                self.last = now;
                if now.duration_since(self.start).is_ok_and(|length| length >= SPAN) {
                    self.close();
                }
            }
            _ => {
                self.close();
                self.start = now.max(self.last);
                self.last = self.start;
            }
        }
        self.ticked = true;
    }

    /// Ends the span at `now`, as when the process stops.
    pub(super) fn end(&mut self, now: SystemTime) {
        if now > self.last {
            self.last = now;
        }
        self.close();
    }

    /// Records the span up to the last count, carrying partial player seconds over, and starts the next one there.
    fn close(&mut self) {
        if self.last <= self.start {
            return;
        }
        self.index += 1;
        let player_seconds = u64::try_from(self.player_millis / 1000).unwrap_or(u64::MAX);
        self.player_millis %= 1000;
        if self.pending.len() == MAX_PENDING {
            self.pending.pop_front();
        }
        self.pending.push_back(v1::UsageRecord {
            id: format!("{}/{}", self.instance_id, self.index),
            start_time: Some(self.start.into()),
            end_time: Some(self.last.into()),
            player_seconds,
        });
        self.start = self.last;
    }

    /// The oldest records, up to `count`.
    pub(super) fn pending(&self, count: usize) -> Vec<v1::UsageRecord> {
        self.pending.iter().take(count).cloned().collect()
    }

    /// Forgets the records management took, by ID.
    pub(super) fn delivered(&mut self, records: &[v1::UsageRecord]) {
        self.pending.retain(|record| !records.iter().any(|delivered| delivered.id == record.id));
    }

    /// Whether every recorded span reached management.
    pub(super) fn settled(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_split_at_a_gap_and_count_player_time_only_while_awake() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let at = |seconds| start + Duration::from_secs(seconds);
        let mut usage = Usage::new("core".into(), start);
        // A slow start still counts, then two players for a minute fill the first span.
        usage.tick(at(20), 0);
        for second in 21..=60 {
            usage.tick(at(second), 2);
        }
        usage.tick(at(61), 1);
        // Suspended from 61 to 3600: the second span ends at 61.
        usage.tick(at(3_600), 1);
        usage.tick(at(3_601), 1);
        usage.end(at(3_602));
        let records = usage.pending(10);
        let spans: Vec<_> = records
            .iter()
            .map(|record| {
                let [from, to] = [&record.start_time, &record.end_time]
                    .map(|time| SystemTime::try_from(time.unwrap()).unwrap().duration_since(start).unwrap());
                (record.id.as_str(), from.as_secs(), to.as_secs(), record.player_seconds)
            })
            .collect();
        assert_eq!(spans, [("core/1", 0, 60, 80), ("core/2", 60, 61, 1), ("core/3", 3_600, 3_602, 1)]);

        // A retried batch is forgotten only once delivered, and never recorded twice.
        usage.delivered(&records[..2]);
        assert_eq!(usage.pending(10), records[2..]);
        assert!(!usage.settled());
        usage.delivered(&records);
        assert!(usage.settled());
    }
}
