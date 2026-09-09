//! JVM service configuration shared by standalone and embedded hosts.
use crate::{DeploymentRef, Launch, ManagedJvm, Phase, RuntimeConnection};
use std::{io, path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;

pub struct Config {
    pub distribution: PathBuf,
    pub java: PathBuf,
    pub connection: PathBuf,
    pub deployment: DeploymentRef,
    pub machine_profile: String,
    pub artifact_digest: String,
    pub memory_mib: u32,
    pub backend: Option<chunk_contract::BackendConnection>,
}
impl Config {
    /// Starts an empty JVM and returns ownership after authenticated readiness.
    /// # Errors
    /// Reports invalid configuration and JVM startup errors.
    pub async fn launch(&self) -> io::Result<ManagedJvm> {
        if !(128..=8192).contains(&self.memory_mib) {
            return Err(io::Error::other("invalid JVM memory"));
        }
        if let Some(parent) = self.connection.parent() {
            std::fs::create_dir_all(parent)?;
        }
        ManagedJvm::launch(Launch {
            backend: self.backend.clone(),
            program: self.java.clone(),
            arguments: vec![
                format!("-Xmx{}M", self.memory_mib),
                "-cp".into(),
                self.distribution.join("lib/*").to_string_lossy().into_owned(),
                "dev.chunkzero.runtime.BridgeMainKt".into(),
            ],
            deployment: self.deployment.clone(),
            machine_profile: self.machine_profile.clone(),
            artifact_digest: self.artifact_digest.clone(),
            log_path: self.connection.with_extension("log"),
            startup_timeout: Duration::from_secs(30),
            bootstrap_session: false,
        })
        .await
    }
}

#[must_use]
pub fn connection(process: &ManagedJvm) -> RuntimeConnection {
    RuntimeConnection {
        endpoint: process.endpoint().into(),
        token: process.credential().into(),
        identity: process.identity().clone(),
    }
}

/// Runs one independently hosted runtime until stopped, recording only confirmed exits.
/// # Errors
/// Reports startup, discovery, lifecycle and shutdown errors.
pub async fn run(config: Config, stop: CancellationToken) -> io::Result<()> {
    if config.connection.exists() || config.connection.with_extension("exit").exists() {
        return Err(io::Error::other("runtime identity already used"));
    }
    let process = config.launch().await?;
    let result = async {
        let _record = chunk_service::Record::publish(&config.connection, &connection(&process))?;
        let mut status = process.watch();
        tokio::select! {
            () = stop.cancelled() => Ok(()),
            state = status.wait_for(|state| matches!(state.phase, Phase::Stopped | Phase::Failed)) => {
                match state { Ok(state) if state.phase == Phase::Stopped => Ok(()), _ => Err(io::Error::other("JVM failed")) }
            }
        }
    }.await;
    process.stop().await?;
    std::fs::write(config.connection.with_extension("exit"), b"stopped")?;
    result
}
