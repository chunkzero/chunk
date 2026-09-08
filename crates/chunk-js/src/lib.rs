//! Isolated transactional JavaScript execution, with no ambient runtime I/O.
//! Each deployment retains a V8 isolate; invocation snapshot capabilities expire.
//! Writes remain speculative; only the environment backend can validate/commit.

mod capabilities;
mod deadline;
mod deployment;
mod model;
mod runtime;

pub use deployment::Deployment;
pub use model::{Cancellation, Error, Execution, Invocation, Key, Limits, Mode, Read, ReadHost, Write};

#[cfg(test)]
mod tests;
