//! Development host owning Rust supervisors directly and JVMs as child processes.
use crate::{Error, Host, MachineProfile, Result};
use chunk_runtime::{ManagedJvm, Phase, RuntimeConnection, Status};
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{Mutex as AsyncMutex, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

pub struct EmbeddedHost {
    config: chunk_runtime::server::Config,
    profiles: BTreeMap<String, MachineProfile>,
    processes: Mutex<BTreeMap<String, Arc<AsyncMutex<Option<OwnedRuntime>>>>>,
}
struct OwnedRuntime {
    connection: RuntimeConnection,
    status: watch::Receiver<Status>,
    stop: CancellationToken,
    task: Option<JoinHandle<io::Result<()>>>,
}

impl OwnedRuntime {
    fn new(mut process: ManagedJvm, path: std::path::PathBuf) -> Self {
        let connection = chunk_runtime::server::connection(&process);
        let status = process.watch();
        let stop = CancellationToken::new();
        let cancellation = stop.clone();
        let task = tokio::spawn(async move {
            tokio::select! {
                () = cancellation.cancelled() => process.stop().await?,
                result = process.wait() => result?,
            }
            std::fs::write(path.with_extension("exit"), b"stopped")
        });
        Self {
            connection,
            status,
            stop,
            task: Some(task),
        }
    }

    fn finished(&self) -> bool {
        matches!(self.status.borrow().phase, Phase::Failed | Phase::Stopped)
            || self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }

    async fn stop(&mut self) -> Result<()> {
        self.stop.cancel();
        let task = self
            .task
            .as_mut()
            .ok_or(Error::Unresolved("runtime shutdown not confirmed"))?;
        let result = task.await;
        self.task.take();
        result.map_err(io::Error::other)??;
        Ok(())
    }
}

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

impl EmbeddedHost {
    #[must_use]
    pub fn new(config: chunk_runtime::server::Config, profiles: BTreeMap<String, MachineProfile>) -> Self {
        Self {
            config,
            profiles,
            processes: Mutex::new(BTreeMap::new()),
        }
    }
    fn path(&self, id: &str) -> Result<std::path::PathBuf> {
        uuid::Uuid::parse_str(id).map_err(|_| Error::Invalid("invalid host ID"))?;
        Ok(self.config.connection.with_file_name(id).with_extension("json"))
    }
    /// Stops all owned JVMs, including launches whose provisioning did not finish.
    /// # Errors
    /// Reports unconfirmed JVM exits or record errors.
    pub async fn shutdown(&self) -> Result<()> {
        let ids: Vec<_> = self
            .processes
            .lock()
            .map_err(|_| Error::Unresolved("host poisoned"))?
            .keys()
            .cloned()
            .collect();
        let mut result = Ok(());
        for id in ids {
            if let Err(error) = self.terminate(&id).await {
                result = Err(error);
            }
        }
        result
    }
}
#[tonic::async_trait]
impl Host for EmbeddedHost {
    async fn ensure(&self, id: &str, profile: &str) -> Result<RuntimeConnection> {
        let path = self.path(id)?;
        let memory = self
            .profiles
            .get(profile)
            .ok_or(Error::Invalid("unknown machine profile"))?
            .memory_mib;
        let entry = self
            .processes
            .lock()
            .map_err(|_| Error::Unresolved("host poisoned"))?
            .entry(id.into())
            .or_default()
            .clone();
        let mut process = entry.lock().await;
        if let Some(running) = process.as_mut() {
            if running.finished() {
                running.stop().await?;
                *process = None;
                return Err(Error::Stopped);
            }
            return Ok(running.connection.clone());
        }
        if path.with_extension("exit").exists() {
            return Err(Error::Stopped);
        }
        std::fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("runtime directory"))?)?;
        // Existing launch records cannot establish ownership after a dev-process crash.
        chunk_service::private_file(&path.with_extension("launch"))?;
        let config = chunk_runtime::server::Config {
            distribution: self.config.distribution.clone(),
            java: self.config.java.clone(),
            connection: path.clone(),
            deployment: self.config.deployment.clone(),
            machine_profile: profile.into(),
            artifact_digest: self.config.artifact_digest.clone(),
            memory_mib: memory,
            backend: self.config.backend.clone(),
        };
        let running = config.launch().await?;
        let running = OwnedRuntime::new(running, path);
        let connection = running.connection.clone();
        *process = Some(running);
        Ok(connection)
    }
    async fn terminate(&self, id: &str) -> Result<()> {
        let path = self.path(id)?;
        let entry = self
            .processes
            .lock()
            .map_err(|_| Error::Unresolved("host poisoned"))?
            .get(id)
            .cloned();
        if let Some(entry) = entry {
            let mut process = entry.lock().await;
            if let Some(running) = process.as_mut() {
                running.stop().await?;
                *process = None;
            }
        }
        if path.with_extension("exit").is_file() {
            Ok(())
        } else {
            Err(Error::Unresolved("runtime launch has no confirmed outcome"))
        }
    }
    fn stopped(&self, id: &str) -> bool {
        self.path(id).is_ok_and(|path| path.with_extension("exit").is_file())
    }
}

#[cfg(test)]
mod tests;
