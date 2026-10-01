//! One environment engine thread owns mutations and speculative state, and read engines
//! run queries and subscriptions. Durable storage runs on a commit thread; replies and
//! updates wait for its ordered acks.

/// Logs a line JavaScript wrote through `console` at its method's level, so filters and log capture see its severity.
macro_rules! console {
    ($level:expr, $($fields:tt)*) => {
        match $level {
            "error" => tracing::error!(target: "chunk_backend::console", $($fields)*),
            "warn" => tracing::warn!(target: "chunk_backend::console", $($fields)*),
            "debug" => tracing::debug!(target: "chunk_backend::console", $($fields)*),
            _ => tracing::info!(target: "chunk_backend::console", $($fields)*),
        }
    };
}

mod actions;
mod actor;
mod commands;
mod commit;
mod effects;
mod evaluate;
mod hooks;
mod limits;
mod moves;
mod reads;
mod send;
pub mod server;
mod service;
mod system;
mod timing;

pub use actions::{ActionHandle, ActionId, ActionIdentity, ActionStatus};
pub use actor::MAX_DEPLOYMENTS;
pub use chunk_js::DeploymentId;
pub use commands::{
    CommandCatalog, CommandEffect, CommandEffects, CommandIdentity, CommandRequest, CommandScope,
    CommandSuggestionRequest,
};
pub use effects::{ActionEffects, Secrets};
pub use limits::Limit;
pub use moves::PlayerMoves;
pub use send::{SendBudget, SendCharge};
pub use service::{
    Backend, Call, GroupSubscription, GroupUpdate, Progress, Readiness, RequestCharge, Subscription, Update,
};
pub use system::{ScopeLock, System};
#[cfg(feature = "bench-support")]
pub use timing::{Phase, observe};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, thiserror::Error)]
pub enum Error {
    #[error("backend capacity reached")]
    Busy,
    #[error("backend overloaded: {0}")]
    Overloaded(Limit),
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
    #[error("deployment is not ready")]
    NotReady,
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
        match error {
            chunk_store::Error::JobBudget => Limit::Jobs.exceeded(),
            error => Self::Storage(error.into()),
        }
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

impl Error {
    pub(crate) fn is_rejected_commit(&self) -> bool {
        matches!(self, Self::Overloaded(Limit::Jobs))
            || matches!(self, Self::Storage(error) if matches!(error.as_ref(), chunk_store::Error::Conflict { .. } | chunk_store::Error::Invalid(_) | chunk_store::Error::Capacity | chunk_store::Error::OperationMismatch | chunk_store::Error::RolledBack(_)))
    }
}

#[cfg(test)]
mod tests;
