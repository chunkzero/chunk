//! A bounded in-memory log buffer fed by a `tracing` layer and read by the dashboard.

use std::{
    collections::VecDeque,
    fmt::Write as _,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tracing::{Event, Subscriber, field::Visit};
use tracing_subscriber::{Layer, layer::Context};

const CAPACITY: usize = 1000;

#[derive(Clone, Serialize)]
pub struct Entry {
    pub seq: u64,
    pub time_ms: u64,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

/// The most recent log events, each numbered so clients can poll for what they missed.
#[derive(Default)]
pub struct Logs {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    next: u64,
    entries: VecDeque<Entry>,
}

impl Logs {
    pub fn record(&self, level: &'static str, target: &str, message: String) {
        let time_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or_default();
        let mut inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.next += 1;
        let entry = Entry {
            seq: inner.next,
            time_ms,
            level,
            target: target.to_owned(),
            message,
        };
        if inner.entries.len() == CAPACITY {
            inner.entries.pop_front();
        }
        inner.entries.push_back(entry);
    }

    /// Entries newer than `after`, oldest first.
    #[must_use]
    pub fn since(&self, after: u64) -> Vec<Entry> {
        let inner = self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .entries
            .iter()
            .filter(|entry| entry.seq > after)
            .cloned()
            .collect()
    }
}

/// A `tracing` layer that copies every event into a [`Logs`] buffer.
pub struct LogLayer(pub std::sync::Arc<Logs>);

impl<S: Subscriber> Layer<S> for LogLayer {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let mut visitor = Message::default();
        event.record(&mut visitor);
        let metadata = event.metadata();
        self.0.record(metadata.level().as_str(), metadata.target(), visitor.0);
    }
}

#[derive(Default)]
struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let rest = std::mem::take(&mut self.0);
            let _ = write!(self.0, "{value:?}");
            if !rest.is_empty() {
                self.0.push(' ');
                self.0.push_str(&rest);
            }
        } else {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            let rest = std::mem::take(&mut self.0);
            self.0.push_str(value);
            if !rest.is_empty() {
                self.0.push(' ');
                self.0.push_str(&rest);
            }
        } else {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = write!(self.0, "{}={value}", field.name());
        }
    }
}
