use std::{
    collections::BTreeMap,
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use chunk_runtime::RuntimeConnection;
use serde::{Deserialize, Serialize};
use tokio::{
    process::Command,
    time::{Instant, sleep},
};

use crate::{Error, Result};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    pub memory_mib: u32,
    pub max_sessions: u16,
}

/// Host effects use a stable ID allocated by the durable control authority.
#[tonic::async_trait]
pub trait Host: Send + Sync {
    async fn ensure(&self, id: &str, profile: &str) -> Result<RuntimeConnection>;
    /// Idempotently stops only this runtime and its descendants.
    /// Success guarantees `stopped(id)`.
    async fn terminate(&self, id: &str) -> Result<()>;
    /// True requires affirmative evidence of complete runtime/JVM shutdown.
    fn stopped(&self, id: &str) -> bool;
}

pub struct ProcessHost {
    pub program: PathBuf,
    pub distribution: PathBuf,
    pub java: PathBuf,
    pub directory: PathBuf,
    pub deployment: chunk_proto::v1::DeploymentRef,
    pub artifact_digest: String,
    pub profiles: BTreeMap<String, MachineProfile>,
    pub backend: Option<chunk_contract::BackendConnection>,
}

impl ProcessHost {
    fn path(&self, id: &str, extension: &str) -> Result<PathBuf> {
        uuid::Uuid::parse_str(id).map_err(|_| Error::Invalid("invalid host ID"))?;
        Ok(self.directory.join(id).with_extension(extension))
    }
}

#[tonic::async_trait]
impl Host for ProcessHost {
    async fn ensure(&self, id: &str, profile: &str) -> Result<RuntimeConnection> {
        let size = self
            .profiles
            .get(profile)
            .ok_or(Error::Invalid("unknown machine profile"))?;
        if let Some(backend) = &self.backend
            && (backend.environment != self.deployment.environment || backend.deployment != self.deployment.deployment)
        {
            return Err(Error::Invalid("gameplay backend scope mismatch"));
        }
        let record = self.path(id, "json")?;
        std::fs::create_dir_all(&self.directory)?;
        let marker = private_file(&self.path(id, "launch")?);
        match marker {
            Ok(_) => {
                let log = private_file(&self.path(id, "supervisor.log")?)?;
                let mut pid_file = private_file(&self.path(id, "pid")?)?;
                let mut command = Command::new(&self.program);
                #[cfg(unix)]
                command.process_group(0);
                let backend_path = self.directory.join(format!("{id}.backend"));
                if let Some(backend) = &self.backend {
                    private_file(&backend_path)?.write_all(&serde_json::to_vec(backend)?)?;
                    command.env("CHUNK_BACKEND_FILE", &backend_path);
                } else {
                    command.env_remove("CHUNK_BACKEND_FILE");
                }
                let child = command
                    .env("CHUNK_CONNECTION", &record)
                    .env("CHUNK_DISTRIBUTION", &self.distribution)
                    .env("CHUNK_JAVA", &self.java)
                    .env("CHUNK_ENVIRONMENT", &self.deployment.environment)
                    .env("CHUNK_DEPLOYMENT", &self.deployment.deployment)
                    .env("CHUNK_MACHINE_PROFILE", profile)
                    .env("CHUNK_ARTIFACT_DIGEST", &self.artifact_digest)
                    .env("CHUNK_MEMORY_MIB", size.memory_mib.to_string())
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(log.try_clone()?))
                    .stderr(Stdio::from(log))
                    .spawn();
                match child {
                    Ok(mut child) => {
                        let pid = child.id().ok_or(Error::Unresolved("supervisor has no PID"))?;
                        if let Err(error) = pid_file.write_all(pid.to_string().as_bytes()) {
                            #[cfg(unix)]
                            signal_group(pid, "-KILL").await;
                            let _ = child.kill().await;
                            return Err(error.into());
                        }
                        tokio::spawn(async move {
                            let _ = child.wait().await;
                        });
                    }
                    Err(error) => {
                        std::fs::write(self.path(id, "exit")?, b"spawn failed")?;
                        return Err(error.into());
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let deadline = Instant::now() + Duration::from_secs(35);
        loop {
            if self.stopped(id) {
                return Err(Error::Stopped);
            }
            if let Ok(bytes) = std::fs::read(&record)
                && let Ok(connection) = serde_json::from_slice::<RuntimeConnection>(&bytes)
            {
                if connection.identity.deployment.as_ref() != Some(&self.deployment)
                    || connection.identity.machine_profile != profile
                    || connection.identity.artifact_digest != self.artifact_digest
                {
                    return Err(Error::Invalid("runtime identity mismatch"));
                }
                return Ok(connection);
            }
            if Instant::now() >= deadline {
                return Err(Error::Unresolved("runtime launch has no confirmed outcome"));
            }
            sleep(Duration::from_millis(25)).await;
        }
    }

    async fn terminate(&self, id: &str) -> Result<()> {
        terminate_runtime(&self.directory, id).await
    }

    fn stopped(&self, id: &str) -> bool {
        self.path(id, "exit").is_ok_and(|path| path.is_file())
    }
}

/// Requests authenticated termination and requires an exit acknowledgment.
/// Persisted numeric PIDs are diagnostic data, never authority to signal a process.
/// # Errors
/// Reports invalid identity, unavailable runtime control or unconfirmed shutdown.
pub async fn terminate_runtime(directory: &Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map_err(|_| Error::Invalid("invalid host ID"))?;
    let path = directory.join(id).with_extension("json");
    let exit = path.with_extension("exit");
    if exit.is_file() {
        return Ok(());
    }
    match chunk_service::read::<RuntimeConnection>(&path) {
        Ok(connection) => request_stop(connection).await?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    while !exit.is_file() {
        if Instant::now() >= deadline {
            return Err(Error::Unresolved("runtime shutdown not confirmed"));
        }
        sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn request_stop(connection: RuntimeConnection) -> Result<()> {
    let address: std::net::SocketAddr = connection
        .endpoint
        .strip_prefix("http://")
        .ok_or(Error::Invalid("runtime URL"))?
        .parse()
        .map_err(|_| Error::Invalid("runtime address"))?;
    if !address.ip().is_loopback() {
        return Err(Error::Invalid("runtime requires loopback"));
    }
    let mut request = tonic::Request::new(connection.identity);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", connection.token)
            .parse()
            .map_err(|_| Error::Invalid("runtime token"))?,
    );
    let Ok(channel) = tonic::transport::Channel::from_shared(connection.endpoint)
        .map_err(|_| Error::Invalid("runtime URL"))?
        .connect_timeout(Duration::from_secs(3))
        .connect()
        .await
    else {
        // A self-stopping runtime may close transport before publishing its exit.
        return Ok(());
    };
    let mut client = chunk_proto::v1::process_control_client::ProcessControlClient::new(channel);
    let rpc = client.stop_process(request);
    // The server may close its transport while completing shutdown.
    let _ = tokio::time::timeout(Duration::from_secs(10), rpc).await;
    Ok(())
}

#[cfg(unix)]
async fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .status()
        .await;
}

pub(crate) use chunk_service::private_file;

#[cfg(test)]
mod tests {
    #[tokio::test(start_paused = true)]
    async fn stale_pid_without_runtime_authority_never_records_shutdown() {
        let directory = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            directory.path().join(format!("{id}.pid")),
            std::process::id().to_string(),
        )
        .unwrap();
        assert!(super::terminate_runtime(directory.path(), &id).await.is_err());
        assert!(!directory.path().join(format!("{id}.exit")).exists());
    }
    #[tokio::test]
    async fn unavailable_runtime_waits_for_confirmed_exit() {
        let directory = tempfile::tempdir().unwrap();
        for has_connection in [false, true] {
            let id = uuid::Uuid::new_v4().to_string();
            let exit = directory.path().join(format!("{id}.exit"));
            if has_connection {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                let connection = chunk_runtime::RuntimeConnection {
                    endpoint: format!("http://{}", listener.local_addr().unwrap()),
                    token: "test".into(),
                    identity: chunk_proto::v1::ProcessIdentity::default(),
                };
                std::fs::write(exit.with_extension("json"), serde_json::to_vec(&connection).unwrap()).unwrap();
            }
            let acknowledge = async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                std::fs::write(&exit, b"stopped").unwrap();
            };
            let (result, ()) = tokio::join!(super::terminate_runtime(directory.path(), &id), acknowledge);
            result.unwrap();
        }
    }
}
