//! Scoped local JVM supervision and authenticated per-player TCP relays.

mod launch;
mod relay;
mod service;
mod wire;

pub use chunk_proto::v1::DeploymentRef;

#[cfg(test)]
mod tests;
pub use launch::{Launch, ManagedJvm, Phase, Status};

/// Private local connection file. Never include this record in diagnostics.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
    pub identity: chunk_proto::v1::ProcessIdentity,
}
