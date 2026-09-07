//! One environment authority: schema validation, optimistic transactions and subscriptions.

mod engine;
mod reads;
mod service;

pub use engine::{Backend, Call, QueryGroup, Update};
pub use service::Service;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid backend request: {0}")]
    Invalid(&'static str),
    #[error("unknown deployment or function")]
    Unknown,
    #[error("request violates its deployment contract")]
    Contract,
    #[error("backend capacity or retry limit reached")]
    Busy,
    #[error("execution was cancelled")]
    Cancelled,
    #[error("backend state poisoned")]
    Poisoned,
    #[error(transparent)]
    Store(#[from] chunk_store::Error),
    #[error(transparent)]
    JavaScript(#[from] chunk_js::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("execution worker stopped: {0}")]
    Worker(#[from] tokio::task::JoinError),
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;
