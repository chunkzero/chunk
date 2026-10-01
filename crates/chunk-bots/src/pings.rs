//! A bot's outstanding play pings, so slow responses are measured instead of dropped.
use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant;

/// How long a ping may go unanswered before it counts as timed out.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// The most pings kept outstanding; beyond it the oldest counts as timed out.
const MAX_OUTSTANDING: usize = 256;

#[derive(Default)]
pub struct Pings {
    last_id: i64,
    /// Oldest first, as sent.
    outstanding: VecDeque<(i64, Instant)>,
}

impl Pings {
    /// Records a ping sent at `now`, returning its ID and how many older pings it pushed out as timed out.
    pub fn sent(&mut self, now: Instant) -> (i64, u64) {
        let pushed_out = u64::from(self.outstanding.len() >= MAX_OUTSTANDING);
        if pushed_out > 0 {
            self.outstanding.pop_front();
        }
        self.last_id += 1;
        self.outstanding.push_back((self.last_id, now));
        (self.last_id, pushed_out)
    }

    /// The round trip of ping `id` answered at `now`, or None if it already timed out or was never sent.
    pub fn answered(&mut self, id: i64, now: Instant) -> Option<Duration> {
        let index = self.outstanding.iter().position(|(sent_id, _)| *sent_id == id)?;
        let (_, sent) = self.outstanding.remove(index)?;
        Some(now.saturating_duration_since(sent))
    }

    /// Drops pings unanswered for over [`TIMEOUT`] at `now`, returning how many.
    pub fn expire(&mut self, now: Instant) -> u64 {
        let mut expired = 0;
        while self.outstanding.front().is_some_and(|(_, sent)| now.saturating_duration_since(*sent) > TIMEOUT) {
            self.outstanding.pop_front();
            expired += 1;
        }
        expired
    }

    /// Forgets every outstanding ping, as when the server that would answer them is gone.
    pub fn clear(&mut self) {
        self.outstanding.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slow_answers_are_kept_until_the_timeout() {
        let start = Instant::now();
        let mut pings = Pings::default();
        let (first, _) = pings.sent(start);
        let (second, _) = pings.sent(start + Duration::from_secs(1));
        assert_eq!(pings.answered(second, start + Duration::from_secs(4)), Some(Duration::from_secs(3)));
        assert_eq!(pings.expire(start + Duration::from_secs(31)), 1);
        assert_eq!(pings.answered(first, start + Duration::from_secs(32)), None);
    }

    #[test]
    fn outstanding_pings_are_bounded() {
        let start = Instant::now();
        let mut pings = Pings::default();
        let pushed_out: u64 = (0..MAX_OUTSTANDING + 3).map(|_| pings.sent(start).1).sum();
        assert_eq!((pings.outstanding.len(), pushed_out), (MAX_OUTSTANDING, 3));
    }
}
