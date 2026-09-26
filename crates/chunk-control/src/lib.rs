//! Durable local placement and player ownership, independent of gameplay data.

mod client;
mod delivery;
mod drain;
mod host;
mod idle;
mod moves;
mod nodes;
mod placement;
mod players;
mod process;
mod reconcile;
mod recovery;
mod roster;
mod rpc;
pub mod server;
mod session_methods;
mod sessions;
mod state;
pub use session_methods::{CapturedSession, PreparedSessionMethod};

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

use chunk_proto::v1::DeploymentRef;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex as AsyncMutex;

pub use host::{Host, MachineProfile, ProcessHostConfig, RuntimeConnection};
pub use process::ProcessHost;
pub use roster::{RosterMember, RosterMove};
pub use rpc::Service;
pub use state::feed::{Change, Table};
use state::{Authority, State};
pub use state::{Generation, clear};

pub use chunk_contract::ControlConnection;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionType {
    pub app: String,
    pub machine_profile: String,
    pub capacity: u32,
}

pub const DEFAULT_IDLE_NODE_TIMEOUT_SECONDS: u32 = 60;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub apps: BTreeMap<String, chunk_contract::AppArtifact>,
    pub deployment: DeploymentRef,
    pub artifact_digest: String,
    pub profiles: BTreeMap<String, MachineProfile>,
    pub session_types: BTreeMap<String, SessionType>,
    pub max_processes: u16,
    /// Seconds a node may run without unfinished sessions before it is stopped; zero keeps idle nodes.
    pub idle_node_timeout_seconds: u32,
    #[serde(flatten)]
    pub contracts: Contracts,
}

/// Optional manifests carried over from the deployment contract.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contracts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_methods: Option<chunk_contract::SessionMethods>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_configurations: Option<chunk_contract::SessionConfigurations>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destinations: Option<chunk_contract::DestinationManifest>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.deployment.environment.is_empty()
            || self.deployment.environment.len() > 100
            || self.deployment.deployment.is_empty()
            || self.artifact_digest.is_empty()
            || self.max_processes == 0
            || self.max_processes > 32
            || self.idle_node_timeout_seconds > 3600
            || self.session_types.is_empty()
            || self
                .profiles
                .values()
                .any(|p| !(128..=8192).contains(&p.memory_mib) || !(1..=16).contains(&p.max_sessions))
            || self.session_types.values().any(|s| {
                !self.apps.contains_key(&s.app)
                    || !(1..=128).contains(&s.capacity)
                    || !self.profiles.contains_key(&s.machine_profile)
            })
        {
            return Err(Error::Invalid("invalid local control configuration"));
        }
        let mut expected = BTreeMap::new();
        for (id, app) in &self.apps {
            app.validate().map_err(|_| Error::Invalid("invalid app manifest"))?;
            if *id != app.id {
                return Err(Error::Invalid("app identity mismatch"));
            }
            for (session, spec) in &app.sessions {
                expected.insert(
                    format!("{id}/{session}"),
                    SessionType {
                        app: id.clone(),
                        machine_profile: spec.machine_profile.clone(),
                        capacity: spec.capacity,
                    },
                );
            }
        }
        let contracts = &self.contracts;
        if let Some(destinations) = &contracts.destinations {
            destinations.validate_apps(&self.apps).map_err(Error::Invalid)?;
            destinations.validate_configurations(contracts.session_configurations.as_ref()).map_err(Error::Invalid)?;
            if destinations
                .entries
                .values()
                .any(|policy| !self.profiles.contains_key(&policy.destination.machine_profile))
            {
                return Err(Error::Invalid("destination references unknown machine profile"));
            }
        }
        if let Some(configurations) = &contracts.session_configurations {
            configurations.validate_apps(&self.apps).map_err(Error::Invalid)?;
        }
        if self.session_types != expected {
            return Err(Error::Invalid("session catalog differs from app manifests"));
        }
        if let Some(methods) = &contracts.session_methods {
            methods.validate().map_err(Error::Invalid)?;
            if serde_json::to_vec(methods)?.len() > 2 * 1024 * 1024
                || methods.methods.iter().any(|method| {
                    self.session_types
                        .get(&format!("{}/{}", method.app, method.session))
                        .is_none_or(|session| session.app != method.app)
                })
            {
                return Err(Error::Invalid("session method catalog differs from app manifests"));
            }
        }
        Ok(())
    }
}

pub struct Control {
    config: Config,
    host: Arc<dyn Host>,
    authority: Authority,
    operations: Mutex<BTreeMap<String, Arc<AsyncMutex<()>>>>,
    draining: std::sync::atomic::AtomicBool,
    observations: Mutex<BTreeMap<String, nodes::Observation>>,
    recovery: recovery::Recovery,
}

impl Control {
    /// Opens one durable authority over its deployment's rows in the environment's system tables. Runtime processes
    /// and their sockets are owned separately.
    /// # Errors
    /// Rejects changed configuration, invalid limits, corrupt state, or a stopped environment store.
    pub fn open(system: chunk_backend::System, config: Config, host: Arc<dyn Host>) -> Result<Arc<Self>> {
        config.validate()?;
        let authority = Authority::open(system, &config)?;
        // A restore can lose a host's row while the JVM launched for it still runs.
        let mut surviving: BTreeSet<_> = authority.read()?.hosts.keys().cloned().collect();
        surviving.extend(host.unowned()?);
        surviving.retain(|id| !host.stopped(id));
        Ok(Arc::new(Self {
            recovery: recovery::Recovery::new(surviving),
            config,
            host,
            observations: Mutex::default(),
            authority,
            operations: Mutex::default(),
            draining: std::sync::atomic::AtomicBool::new(false),
        }))
    }

    fn state(&self) -> Result<Arc<State>> {
        self.authority.read()
    }

    fn update<T>(&self, change: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        self.authority.update(change)
    }

    /// The lock serializing work on one operation. At most 1024 operations are in flight; beyond that, new work
    /// is refused as busy rather than queued.
    fn operation(&self, id: &str) -> Result<Arc<AsyncMutex<()>>> {
        let mut operations = self.operations.lock().map_err(|_| Error::Unresolved("operation mutex poisoned"))?;
        if operations.len() >= 1024 && !operations.contains_key(id) {
            // Only the map holds an idle lock, so dropping it cannot split a caller from its waiters.
            operations.retain(|_, lock| Arc::strong_count(lock) > 1);
            if operations.len() >= 1024 {
                return Err(Error::Busy);
            }
        }
        Ok(operations.entry(id.into()).or_default().clone())
    }

    /// Claim and move changes committed after `position`. `None` means the position is outside retained history
    /// (another epoch, too old, or ahead); reload current state instead.
    #[must_use]
    pub fn changes_after(&self, position: Generation) -> Option<Vec<Change>> {
        self.authority.feed().after(position)
    }

    /// Whether the environment store stopped or failed, so control can no longer commit.
    #[must_use]
    pub fn store_stopped(&self) -> bool {
        self.authority.stopped()
    }

    /// The position of the latest commit, updated after each one is readable.
    #[must_use]
    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<Generation> {
        self.authority.feed().subscribe()
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
    /// Too much work is in flight; retry later.
    #[error("control busy")]
    Busy,
    #[error("runtime and JVM have stopped")]
    Stopped,
    #[error("control store: {0}")]
    Store(chunk_store::Error),
    #[error("environment store: {0}")]
    Backend(chunk_backend::Error),
    #[error("local host I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("control state: {0}")]
    Json(#[from] serde_json::Error),
    #[error("control message: {0}")]
    Decode(#[from] prost::DecodeError),
    #[error("runtime RPC: {0}")]
    Rpc(#[from] tonic::Status),
}

impl From<chunk_store::Error> for Error {
    fn from(error: chunk_store::Error) -> Self {
        match error {
            chunk_store::Error::Capacity => Self::Capacity,
            error => Self::Store(error),
        }
    }
}

impl From<chunk_backend::Error> for Error {
    fn from(error: chunk_backend::Error) -> Self {
        match error {
            chunk_backend::Error::Storage(inner) if matches!(*inner, chunk_store::Error::Capacity) => Self::Capacity,
            error => Self::Backend(error),
        }
    }
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
