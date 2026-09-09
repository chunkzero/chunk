use std::{
    collections::BTreeMap,
    fs::OpenOptions,
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
    /// Idempotently kills only this runtime and its descendants, independently of runtime control.
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
                if let Some(backend) = &self.backend {
                    command
                        .env("CHUNK_BACKEND_ENDPOINT", &backend.endpoint)
                        .env("CHUNK_BACKEND_TOKEN", &backend.token);
                }
                let child = command
                    .arg("runtime")
                    .arg("--managed")
                    .arg("--connection")
                    .arg(&record)
                    .arg("--distribution")
                    .arg(&self.distribution)
                    .arg("--java")
                    .arg(&self.java)
                    .arg("--environment")
                    .arg(&self.deployment.environment)
                    .arg("--deployment")
                    .arg(&self.deployment.deployment)
                    .arg("--machine-profile")
                    .arg(profile)
                    .arg("--artifact-digest")
                    .arg(&self.artifact_digest)
                    .arg("--memory-mib")
                    .arg(size.memory_mib.to_string())
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

/// Terminates a runtime and its descendants using its persisted supervisor PID.
/// # Errors
/// Returns an error for invalid IDs or PIDs, filesystem failures, or unsupported platforms.
pub async fn terminate_runtime(directory: &Path, id: &str) -> Result<()> {
    uuid::Uuid::parse_str(id).map_err(|_| Error::Invalid("invalid host ID"))?;
    let exit = directory.join(id).with_extension("exit");
    if exit.is_file() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let pid = std::fs::read_to_string(directory.join(id).with_extension("pid"))?
            .parse::<u32>()
            .ok()
            .filter(|pid| (2..=i32::MAX as u32).contains(pid))
            .ok_or(Error::Invalid("invalid supervisor PID"))?;
        signal_group(pid, "-TERM").await;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if exit.is_file() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
        signal_group(pid, "-KILL").await;
        match private_file(&exit) {
            Ok(mut file) => file.write_all(b"terminated")?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = exit;
        Err(Error::Unresolved("host termination requires Unix process groups"))
    }
}

#[cfg(unix)]
async fn signal_group(pid: u32, signal: &str) {
    let _ = Command::new("kill")
        .args([signal, "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}

pub(crate) fn private_file(path: &Path) -> io::Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
