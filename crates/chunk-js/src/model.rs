use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    Get {
        table: String,
        id: String,
    },
    Scan {
        table: String,
        start: Option<String>,
        end: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Write {
    pub key: Key,
    pub value: Option<Value>,
}

/// A memory-only snapshot capability. Implementations must record read dependencies,
/// include the supplied speculative overlay, and perform no external effects.
pub trait ReadHost: Send + 'static {
    /// # Errors
    /// Reports invalid reads or snapshot limits without publishing effects.
    fn read(&mut self, request: Read, overlay: &BTreeMap<Key, Option<Value>>) -> Result<Value, String>;
}

/// One call into an already registered deployment.
pub struct Invocation {
    pub export: String,
    pub arguments: Value,
    pub caller: Value,
    pub mode: Mode,
    /// Milliseconds since the Unix epoch at snapshot acquisition, fixed for retries.
    pub timestamp: i64,
    /// Backend-supplied deterministic seed, fixed for the operation and its retries.
    pub seed: u64,
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub execution: Duration,
    pub heap_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            execution: Duration::from_secs(1),
            heap_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct Execution {
    pub value: Value,
    /// Published only on success; the backend still validates and commits these.
    pub writes: Vec<Write>,
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
    #[error("deployment worker stopped")]
    WorkerStopped,
    #[error("runtime I/O: {0}")]
    Io(#[from] std::io::Error),
}
