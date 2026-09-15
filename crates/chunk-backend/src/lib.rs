//! One environment engine thread owns JS execution and speculative state. Durable
//! storage runs on a commit thread; replies and updates wait for its ordered acks.

mod actions;
mod commands;
pub use commands::CommandService;
mod actor;
mod commit;
mod effects;
mod reads;
mod service;
mod transport;

pub use actions::{ActionHandle, ActionId, ActionStatus};
pub use chunk_js::HttpMethod;
pub use effects::{ActionEffects, ActionGrants, HttpBinding};
pub use service::{Backend, Call, GroupSubscription, GroupUpdate, Subscription, Update};
pub use transport::Service;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("backend capacity reached")]
    Busy,
    #[error("commit rejected; retry the operation")]
    Retry,
    #[error("action outcome unknown; do not retry under a new invocation identity")]
    ActionOutcomeUnknown,
    #[error("backend stopped")]
    Closed,
    #[error("request cancelled")]
    Cancelled,
    #[error("invalid backend request: {0}")]
    Invalid(&'static str),
    #[error("function or document contract mismatch")]
    Contract,
    #[error("unknown or inaccessible function")]
    Unknown,
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
mod integration_tests;
#[cfg(test)]
mod tests;

impl Error {
    pub(crate) fn is_rejected_commit(&self) -> bool {
        matches!(self, Self::Storage(error) if matches!(error.as_ref(), chunk_store::Error::Conflict { .. } | chunk_store::Error::Invalid(_) | chunk_store::Error::Capacity | chunk_store::Error::OperationMismatch))
    }
}

#[cfg(test)]
mod context_tests;
#[cfg(test)]
mod document_tests;

pub mod server;

#[cfg(test)]
mod action_tests;

#[cfg(test)]
mod effect_tests;
mod hooks;
pub use hooks::HookService;

#[cfg(test)]
mod job_tests;
