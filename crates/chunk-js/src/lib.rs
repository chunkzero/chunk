//! Isolated transactional JavaScript execution, with no ambient runtime I/O.
//! Each deployment retains a V8 isolate; invocation snapshot capabilities expire.
//! Writes remain speculative; only the environment backend can validate/commit.

mod actions;
mod allocator;
mod capabilities;
mod deadline;

mod engine;
mod extensions;
mod http;
mod isolate;
mod jobs;
mod model;
mod profile;
mod runtime;
mod termination;

pub use actions::{ActionHost, ActionInvocation};
pub use engine::{DeploymentId, Engine};
pub use http::{HttpMethod, HttpOutcome, HttpRequest};
pub use jobs::ScheduleIntent;
pub use model::{
    Cancellation, Error, Execution, IndexRows, Invocation, Json, Key, Limits, Log, Mode, Read, ReadHost, Write,
};

#[cfg(test)]
mod tests;
