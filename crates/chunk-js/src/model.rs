use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Notify;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Query,
    Mutation,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Key {
    pub table: String,
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Read {
    Get { table: String, id: String },
    Scan { table: String, start: Option<String>, end: Option<String> },
    Index { query: chunk_contract::IndexQuery },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Write {
    pub key: Key,
    pub value: Option<Value>,
}

/// A snapshot capability that records dependencies and performs no external effects.
/// The engine merges this invocation's writes into returned snapshot data.
pub trait ReadHost: 'static {
    /// Derives an opaque stable job identity from the durable mutation operation.
    /// # Errors
    /// Rejects hosts without a durable mutation operation binding.
    fn schedule_id(&self, _sequence: u32) -> Result<String, String> {
        Err("Scheduled jobs require a durable mutation host".into())
    }

    /// Records that the invocation read `ctx.caller`, so its result may differ per caller.
    fn read_caller(&mut self) {}

    /// Records that the invocation read the snapshot time, so its result may change with every commit.
    fn read_time(&mut self) {}

    /// # Errors
    /// Reports invalid keys or snapshot limits without publishing effects.
    fn get(&mut self, key: &Key) -> Result<Option<Value>, String>;
    /// Reads a half-open primary-key interval in ascending ID order.
    /// # Errors
    /// Reports invalid ranges or snapshot limits without publishing effects.
    fn scan(&mut self, table: &str, start: Option<&str>, end: Option<&str>) -> Result<Vec<(String, Value)>, String>;
    /// Returns snapshot candidates and the declaration's index fields. The engine
    /// merges invocation writes and applies the requested order/limit afterward.
    /// # Errors
    /// Rejects undeclared indexes, unsupported bounds or exhausted read budgets.
    fn scan_index(&mut self, _query: &chunk_contract::IndexQuery) -> Result<IndexRows, String> {
        Err("Indexed reads are not supported by this host".into())
    }
}

pub struct IndexRows {
    pub fields: Vec<String>,
    pub rows: Vec<(String, Value)>,
}

/// Canonical JSON text shared without cloning or re-encoding its value tree.
#[derive(Debug, Clone)]
pub struct Json(Arc<str>);

impl Json {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Parses and canonicalizes incoming wire JSON before admission.
    /// # Errors
    /// Rejects invalid JSON, unsupported Unicode and excessive nesting.
    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str::<Value>(text).map(Self::from)
    }

    /// The empty object.
    #[must_use]
    pub fn empty() -> Self {
        Self("{}".into())
    }
}

impl From<Value> for Json {
    fn from(mut value: Value) -> Self {
        value.sort_all_objects();
        Self(serde_json::to_string(&value).expect("JSON value is serializable").into())
    }
}

/// One call into an already registered deployment.
pub struct Invocation {
    pub export: String,
    pub arguments: Json,
    pub caller: Json,
    pub mode: Mode,
    /// Milliseconds since the Unix epoch at snapshot acquisition, fixed for retries.
    pub timestamp: i64,
    /// Backend-supplied deterministic seed, fixed for the operation and its retries.
    pub seed: u64,
}

pub(crate) mod bounds {
    use std::time::Duration;
    pub const NAME_BYTES: usize = 128;
    pub const SOURCE_BYTES: usize = 4 * 1024 * 1024;
    pub const JSON_BYTES: usize = 1024 * 1024;
    pub const READ_REQUEST_BYTES: usize = 4096;
    pub const CAPABILITY_CALLS: usize = 4096;
    pub const TABLE_BYTES: usize = 64;
    pub const DOCUMENT_ID_BYTES: usize = 256;
    pub const INVOCATION_ID_BYTES: usize = 256;
    pub const WRITES: usize = 256;
    pub const WRITE_BYTES: usize = 8 * 1024 * 1024;
    pub const MIN_HEAP_BYTES: usize = 16 * 1024 * 1024;
    pub const MAX_HEAP_BYTES: usize = 128 * 1024 * 1024;
    pub const EMERGENCY_HEAP_BYTES: usize = 8 * 1024 * 1024;
    pub const MAX_EXECUTION: Duration = Duration::from_secs(30);
    pub const RUNTIME_CALLS: u32 = 10_000;
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub execution: Duration,
    pub heap_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { execution: Duration::from_secs(1), heap_bytes: 32 * 1024 * 1024 }
    }
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<(AtomicBool, Notify)>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.0.store(true, Ordering::Release);
        self.0.1.notify_waiters();
    }
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.0.load(Ordering::Acquire)
    }
    /// Resolves once cancelled, without polling.
    pub async fn cancelled(&self) {
        let notified = self.0.1.notified();
        tokio::pin!(notified);
        // Register before checking the flag so a concurrent `cancel` cannot be missed.
        notified.as_mut().enable();
        if !self.is_cancelled() {
            notified.await;
        }
    }
    /// Resolves once cancelled or the deadline passes, whichever comes first.
    pub async fn expired(&self, deadline: Instant) {
        tokio::select! {
            () = self.cancelled() => {}
            () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
        }
    }
}

#[derive(Debug)]
pub struct Execution {
    pub logs: Vec<Log>,
    /// Strict JSON text, ready to forward without decoding on the host.
    pub value: String,
    /// Published only on success; the backend still validates and commits these.
    pub writes: Vec<Write>,
    pub jobs: Vec<crate::ScheduleIntent>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid invocation: {0}")]
    Invalid(&'static str),
    #[error("JavaScript execution cancelled")]
    Cancelled,
    #[error("JavaScript execution deadline exceeded")]
    Deadline,
    #[error("JavaScript heap limit exceeded")]
    Heap,
    #[error("JavaScript: {0}")]
    JavaScript(String),
    #[error("deployment is not registered")]
    UnknownDeployment,
    #[error("runtime I/O: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct Log {
    pub level: String,
    pub message: String,
}

#[cfg(test)]
mod tests;
