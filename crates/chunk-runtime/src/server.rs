//! JVM service configuration shared by standalone and embedded hosts.
use crate::{DeploymentRef, Launch, LaunchError, ManagedJvm, Phase, RuntimeConnection};
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
    /// Reports configuration/startup errors, recording an exit only when no JVM can remain.
    pub async fn launch(&self) -> io::Result<ManagedJvm> {
        match self.start().await {
            Ok(process) => Ok(process),
            Err(error) => Err(record_launch_failure(&self.connection, error)),
        }
    }

    async fn start(&self) -> Result<ManagedJvm, LaunchError> {
        if let Some(parent) = self.connection.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !(128..=8192).contains(&self.memory_mib) {
            return Err(io::Error::other("invalid JVM memory").into());
        }
        ManagedJvm::launch(Launch {
            backend: self.backend.clone(),
            program: self.java.clone(),
            arguments: vec![
                format!("-Xmx{}M", self.memory_mib),
                "-cp".into(),
                self.distribution.join("lib/*").to_string_lossy().into_owned(),
                "dev.chunkzero.runtime.BridgeMain".into(),
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

fn record_launch_failure(connection: &std::path::Path, error: LaunchError) -> io::Error {
    if matches!(error, LaunchError::Stopped(_))
        && let Err(record_error) = std::fs::write(connection.with_extension("exit"), b"launch failed")
    {
        return io::Error::other(format!("{error}; recording failed launch: {record_error}"));
    }
    error.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_standalone_launch_records_confirmed_exit() {
        let directory = tempfile::tempdir().unwrap();
        for memory_mib in [1, 512] {
            let connection = directory.path().join(memory_mib.to_string()).join("runtime.json");
            let config = Config {
                distribution: directory.path().into(),
                java: directory.path().join("missing-java"),
                connection: connection.clone(),
                deployment: DeploymentRef { environment: "local".into(), deployment: "test".into() },
                machine_profile: "test".into(),
                artifact_digest: "test".into(),
                memory_mib,
                backend: None,
            };
            let error = run(config, CancellationToken::new()).await.unwrap_err();
            if memory_mib == 1 {
                assert_eq!(error.to_string(), "invalid JVM memory");
            } else {
                assert_eq!(error.kind(), io::ErrorKind::NotFound);
            }
            assert!(connection.with_extension("exit").is_file());
            assert!(!connection.exists());
        }
    }

    #[test]
    fn ambiguous_launch_cleanup_never_acknowledges_exit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("runtime.json");
        let error = record_launch_failure(&path, LaunchError::Unresolved(io::Error::other("wait failed")));
        assert_eq!(error.to_string(), "wait failed");
        assert!(!path.with_extension("exit").exists());
    }
}
