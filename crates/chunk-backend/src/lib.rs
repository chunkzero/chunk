//! One environment engine thread owns JS execution and speculative state. Durable
//! storage runs on a commit thread; replies and updates wait for its ordered acks.

mod actor;
mod commit;
mod reads;
mod service;

pub use service::{Backend, Call, Subscription, Update};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("backend capacity reached")]
    Busy,
    #[error("backend stopped")]
    Closed,
    #[error("request cancelled")]
    Cancelled,
    #[error("invalid backend request: {0}")]
    Invalid(&'static str),
    #[error("operation ID was reused for a different request")]
    OperationMismatch,
    #[error("commit pipeline failed; recover the operation outcome after restarting the backend")]
    CommitFailed,
    #[error("JavaScript: {0}")]
    JavaScript(std::sync::Arc<chunk_js::Error>),
    #[error("storage: {0}")]
    Storage(std::sync::Arc<chunk_store::Error>),
    #[error("I/O: {0}")]
    Io(std::sync::Arc<std::io::Error>),
    #[error("JSON: {0}")]
    Json(std::sync::Arc<serde_json::Error>),
}

impl From<chunk_js::Error> for Error {
    fn from(error: chunk_js::Error) -> Self {
        Self::JavaScript(error.into())
    }
}
impl From<chunk_store::Error> for Error {
    fn from(error: chunk_store::Error) -> Self {
        Self::Storage(error.into())
    }
}
impl From<std::io::Error> for Error {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.into())
    }
}
impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error.into())
    }
}

#[cfg(test)]
mod tests;
