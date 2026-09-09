use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io,
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
                let mut command = Command::new(&self.program);
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

    fn stopped(&self, id: &str) -> bool {
        self.path(id, "exit").is_ok_and(|path| path.is_file())
    }
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
