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
pub trait ReadHost: 'static {
    /// # Errors
    /// Reports invalid reads or snapshot limits without publishing effects.
    fn read(&mut self, request: Read, overlay: &BTreeMap<Key, Option<Value>>) -> Result<Value, String>;
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
}

impl From<Value> for Json {
    fn from(value: Value) -> Self {
        Self(
            serde_json::to_string(&value)
                .expect("JSON value is serializable")
                .into(),
        )
    }
}

/// One call into an already registered deployment.
pub struct Invocation {
    pub export: String,
    pub arguments: Json,
    pub caller: Json,
    pub mode: Mode,
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
    /// Strict JSON text, ready to forward without decoding on the host.
    pub value: String,
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
    #[error("deployment is not registered")]
    UnknownDeployment,
    #[error("runtime I/O: {0}")]
    Io(#[from] std::io::Error),
}
