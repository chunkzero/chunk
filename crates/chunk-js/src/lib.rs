//! Isolated transactional JavaScript execution, with no ambient runtime I/O.
//! Every invocation gets a fresh V8 isolate and scoped snapshot capabilities.
//! Writes remain speculative; only the environment backend can validate/commit.

mod capabilities;
mod model;
mod runtime;

pub use model::{Cancellation, Error, Execution, Invocation, Key, Limits, Mode, Read, ReadHost, Write};
pub use runtime::execute;

#[cfg(test)]
mod tests;
