use crate::{Error, Release, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    pub memory_mib: u32,
    pub max_sessions: u16,
}

/// Who a JVM is: the host it runs, the process launched for it, and the release, app and profile it runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JvmIdentity {
    pub host: String,
    pub process_id: String,
    pub generation: u64,
    /// The deployment whose release the JVM runs.
    pub deployment: String,
    pub app: String,
    pub profile: String,
    /// The SHA-256 digest of the app's artifact, in lowercase hex.
    pub artifact_digest: String,
}

/// What a JVM registers with: who it is, and the endpoint players connect to.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registration {
    pub identity: JvmIdentity,
    pub player_endpoint: String,
}

/// A registered JVM's credential and identity, and the endpoint players connect to. Control reaches the JVM only
/// through its `jvm/<host>` topic. Never include this record in diagnostics.
#[derive(Clone)]
pub struct RuntimeConnection {
    pub token: String,
    pub identity: JvmIdentity,
    pub player_endpoint: String,
}

/// How far a host has come toward providing a runtime for its capacity request.
pub enum Progress {
    Pending,
    Ready(Box<RuntimeConnection>),
    /// The host can never provide this runtime.
    Failed(String),
}

/// Host effects use a stable ID allocated by the durable control authority. Each ID is one capacity request: one JVM
/// lifetime, never relaunched.
#[tonic::async_trait]
pub trait Host: Send + Sync {
    /// Starts providing `id`'s runtime of `release`'s `app` if it has not started yet, and reports its progress without
    /// waiting. Repeated calls with the same ID never start a second runtime.
    async fn ensure(&self, id: &str, release: &Release, app: &str, profile: &str) -> Result<Progress>;
    /// Stops `id`'s runtime, or confirms it never started. `true` only with affirmative evidence that no process for
    /// `id` runs; `false` asks the caller to try again.
    async fn release(&self, id: &str) -> Result<bool>;
    /// Whether `id`'s runtime is confirmed to have exited.
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
    fn register(&self, _token: &str, _registration: Registration) -> Result<()> {
        Err(Error::Invalid("host does not accept process registrations"))
    }
    /// Takes over a process launched for `registration`'s host before control restarted. Control has already
    /// matched `token` and the identity against its log.
    /// # Errors
    /// Rejects hosts that cannot adopt processes, or a host already running or stopped.
    fn adopt(&self, _token: &str, _registration: Registration) -> Result<()> {
        Err(Error::Invalid("host cannot adopt processes"))
    }
    fn connection(&self, _id: &str) -> Option<RuntimeConnection> {
        None
    }
    /// The host whose running JVM holds `credential`, compared in constant time.
    fn authenticate(&self, _credential: &str) -> Option<String> {
        None
    }
    /// The host whose launch from before control restarted, awaiting re-attachment, was given `credential`, compared
    /// in constant time.
    fn unadopted(&self, _credential: &str) -> Option<String> {
        None
    }
}

pub struct ProcessHostConfig {
    /// Holds each host's launch marker, exit record and JVM log.
    pub directory: std::path::PathBuf,
    /// The environment whose releases this host launches.
    pub environment: String,
    /// This machine's address on the environment's private network. When set, each JVM serves players there, so
    /// gateways on other machines reach it; otherwise it serves them on loopback.
    pub private_address: Option<std::net::IpAddr>,
}

/// Where a release's distribution was unpacked, and the Java executable that runs its apps.
#[derive(Clone)]
pub struct Distribution {
    pub directory: std::path::PathBuf,
    pub java: std::path::PathBuf,
}
