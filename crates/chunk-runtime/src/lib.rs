//! Scoped local JVM supervision and authenticated per-player TCP relays.

mod launch;
mod relay;
mod service;
mod wire;

pub use chunk_proto::v1::DeploymentRef;

#[cfg(test)]
mod tests;
pub use launch::{Launch, ManagedJvm, Phase, Status};
