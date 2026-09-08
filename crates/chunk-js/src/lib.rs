//! Isolated transactional JavaScript execution, with no ambient runtime I/O.
//! Each deployment retains a V8 isolate; invocation snapshot capabilities expire.
//! Writes remain speculative; only the environment backend can validate/commit.

mod allocator;
mod capabilities;
mod deadline;

mod engine;
mod isolate;
mod model;
mod profile;
mod runtime;
mod termination;

pub use engine::{DeploymentId, Engine};
pub use model::{
    Cancellation, Error, Execution, IndexRows, Invocation, Json, Key, Limits, Mode, Read, ReadHost, Write,
};

#[cfg(test)]
mod tests;
