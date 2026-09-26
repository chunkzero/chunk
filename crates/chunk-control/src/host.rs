use crate::{Error, Result};
use chunk_proto::v1::{ProcessIdentity, ProcessRegistration};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    pub memory_mib: u32,
    pub max_sessions: u16,
}

/// Private connection capability. Never include this record in diagnostics.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConnection {
    pub endpoint: String,
    pub token: String,
    pub identity: ProcessIdentity,
    pub player_endpoint: String,
}

/// Host effects use a stable ID allocated by the durable control authority.
#[tonic::async_trait]
pub trait Host: Send + Sync {
    async fn ensure(&self, id: &str, app: &str, profile: &str) -> Result<RuntimeConnection>;
    /// Success requires affirmative evidence of complete process shutdown.
    async fn terminate(&self, id: &str) -> Result<()>;
    fn stopped(&self, id: &str) -> bool;
    /// Removes stopped host resources absent from durable state. The caller must exclude concurrent placement.
    /// # Errors
    /// Failed cleanup remains eligible for the next reconciliation pass.
    fn prune(&self, _retained: &BTreeSet<String>) -> Result<()> {
        Ok(())
    }
    fn unresolved(&self, _id: &str) -> bool {
        false
    }
    /// Hosts whose launch may still run a JVM this host does not own, such as one launched before control restarted.
    /// # Errors
    /// Reports launch records that cannot be listed.
    fn unowned(&self) -> Result<BTreeSet<String>> {
        Ok(BTreeSet::new())
    }
    /// # Errors
    /// Rejects an incompatible control endpoint.
    fn configure(&self, _endpoint: String) -> Result<()> {
        Ok(())
    }
    /// # Errors
    /// Rejects unowned launches, credentials or changed registrations.
    fn register(&self, _token: &str, _registration: ProcessRegistration) -> Result<ProcessIdentity> {
        Err(Error::Invalid("host does not accept process registrations"))
    }
    /// Takes over a process launched for `registration`'s host before control restarted. Control has already
    /// matched `token` and the identity against its log.
    /// # Errors
    /// Rejects hosts that cannot adopt processes, or a host already running or stopped.
    fn adopt(&self, _token: &str, _registration: ProcessRegistration) -> Result<()> {
        Err(Error::Invalid("host cannot adopt processes"))
    }
    fn connection(&self, _id: &str) -> Option<RuntimeConnection> {
        None
    }
}

pub struct ProcessHostConfig {
    pub distribution: std::path::PathBuf,
    pub java: std::path::PathBuf,
    pub directory: std::path::PathBuf,
    pub deployment: chunk_proto::v1::DeploymentRef,
    pub apps: BTreeMap<String, chunk_contract::AppArtifact>,
    pub profiles: BTreeMap<String, MachineProfile>,
    pub backend: chunk_contract::BackendConnection,
}

pub(crate) use chunk_service::private_file;
