//! Durable local placement and player ownership, independent of gameplay data.

mod delivery;
mod drain;
mod host;
mod moves;
mod placement;
mod reconcile;
mod rpc;
mod state;

use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex, MutexGuard},
};

use chunk_proto::v1::DeploymentRef;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

pub use host::{Host, MachineProfile, ProcessHost, terminate_runtime};
pub use rpc::Service;
use state::{Authority, State};

pub use chunk_contract::ControlConnection;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionType {
    pub machine_profile: String,
    pub capacity: u32,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub deployment: DeploymentRef,
    pub artifact_digest: String,
    pub profiles: BTreeMap<String, MachineProfile>,
    pub session_types: BTreeMap<String, SessionType>,
    pub max_processes: u16,
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.deployment.environment.is_empty()
            || self.deployment.environment.len() > 100
            || self.deployment.deployment.is_empty()
            || self.artifact_digest.is_empty()
            || self.max_processes == 0
            || self.max_processes > 32
            || self.session_types.is_empty()
            || self
                .profiles
                .values()
                .any(|p| !(128..=8192).contains(&p.memory_mib) || !(1..=16).contains(&p.max_sessions))
            || self
                .session_types
                .values()
                .any(|s| !(1..=128).contains(&s.capacity) || !self.profiles.contains_key(&s.machine_profile))
        {
            return Err(Error::Invalid("invalid local control configuration"));
        }
        Ok(())
    }
}

pub struct Control {
    config: Config,
    host: Arc<dyn Host>,
    authority: Mutex<Authority>,
    operations: Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
    draining: std::sync::atomic::AtomicBool,
}

impl Control {
    /// Opens one durable authority. Runtime processes and their sockets are owned separately.
    /// # Errors
    /// Rejects a second writer, changed configuration, invalid limits, or corrupt state.
    pub fn open(path: &Path, config: Config, host: Arc<dyn Host>) -> Result<Arc<Self>> {
        config.validate()?;
        let authority = Authority::open(path, &config)?;
        Ok(Arc::new(Self {
            config,
            host,
            authority: Mutex::new(authority),
            operations: Mutex::default(),
            draining: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    fn authority(&self) -> Result<MutexGuard<'_, Authority>> {
        self.authority.lock().map_err(|_| Error::Unresolved("control authority poisoned"))
    }

    fn state(&self) -> Result<State> {
        self.authority()?.read()
    }

    fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.authority()?.update(change)
    }

    fn operation(&self, id: &str) -> Result<Arc<AsyncMutex<()>>> {
        let mut operations = self.operations.lock().map_err(|_| Error::Unresolved("operation mutex poisoned"))?;
        if operations.len() >= 1024 && !operations.contains_key(id) {
            return Err(Error::Capacity);
        }
        Ok(operations.entry(id.into()).or_default().clone())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid control request: {0}")]
    Invalid(&'static str),
    #[error("ownership or provisioning remains unresolved: {0}")]
    Unresolved(&'static str),
    #[error("local control capacity reached")]
    Capacity,
    #[error("runtime and JVM have stopped")]
    Stopped,
    #[error("control storage: {0}")]
    Storage(#[from] chunk_store::Error),
    #[error("local host I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("control state: {0}")]
    Json(#[from] serde_json::Error),
    #[error("control message: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("runtime RPC: {0}")]
    Rpc(#[from] tonic::Status),
}

#[cfg(test)]
mod tests;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub mod server;

mod embedded;
pub use embedded::EmbeddedHost;
