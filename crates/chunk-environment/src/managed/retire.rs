//! Retiring what the environment no longer serves, derived each time from the backend's and control's durable state so
//! that a restart at any point converges. Control starts a replaced deployment's drain at its activation.

use super::{Managed, lock};
use chunk_control::{Control, DrainPolicy};
use chunk_management::v1;
use std::{collections::BTreeSet, convert::Infallible, time::Duration};

/// How replaced deployments drain, from `desired`'s settings; unset, they stop at once.
pub(super) fn drain_policy(desired: &v1::AttachResponse) -> DrainPolicy {
    let settings = desired.drain.unwrap_or_default();
    let seconds = |seconds: u32| Some(Duration::from_secs(seconds.into()));
    DrainPolicy { max_age: seconds(settings.max_age_seconds), deadline: seconds(settings.deadline_seconds) }
}

impl Managed<'_> {
    /// Applies `desired`'s drain settings to every draining deployment, so shortened limits take effect at once.
    pub(super) fn apply_drain_policy(&self, desired: &v1::AttachResponse) {
        let Ok(control) = self.core.control() else { return };
        if let Err(error) = control.set_drain_policy(drain_policy(desired)) {
            tracing::warn!(%error, "drain settings not applied");
        }
    }
}

impl Managed<'_> {
    /// Retires deployments and removes release directories nothing uses any more, every second.
    pub(super) async fn reclaim(&self) -> Infallible {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        loop {
            tick.tick().await;
            self.retire().await;
            self.remove_unused_releases().await;
        }
    }

    /// Retires every deployment the backend holds that is neither control's current one nor kept for management and
    /// that is due or was asked to stop, then releases the backend version of each whose JVMs have all exited, fencing
    /// those asked to stop wherever their jobs and subscriptions stand. A deployment loading while the backend holds as many as it can retires the oldest one to make room.
    pub(super) async fn retire(&self) {
        let (Ok(control), Some(backend)) = (self.core.control(), self.core.backend()) else { return };
        let resident = match backend.deployments().await {
            Ok(resident) => resident,
            Err(error) => return tracing::warn!(%error, "resident deployments unknown"),
        };
        let stopped = self.retire_resident(&control, &resident);
        for (id, asked) in &stopped {
            let released = if *asked { backend.retire(id.clone()).await } else { backend.release(id.clone()).await };
            match released {
                Ok(_) | Err(chunk_backend::Error::Busy) => {}
                Err(error) => tracing::warn!(%error, deployment = id.as_str(), "backend version not released"),
            }
        }
        self.make_room(&control, &backend).await;
    }

    /// Retires the resident deployments that are due or asked to stop, and returns those whose JVMs have all exited,
    /// each with whether it was asked to stop. Checked and retired without an await in between, so management cannot
    /// ask for one meanwhile.
    fn retire_resident(
        &self,
        control: &Control,
        resident: &[chunk_js::DeploymentId],
    ) -> Vec<(chunk_js::DeploymentId, bool)> {
        let deployments = lock(&self.deployments);
        let names: BTreeSet<_> = resident.iter().map(|id| id.as_str().to_owned()).collect();
        let current = match control.current_release() {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(%error, "current release unknown");
                return Vec::new();
            }
        };
        let stopping = control.stopping().unwrap_or_default();
        if stopping.iter().any(|name| !names.contains(name))
            && let Err(error) = control.forget_stopping(&names)
        {
            tracing::warn!(%error, "stopped deployments not forgotten");
        }
        let due = control.due_releases().unwrap_or_default();
        let mut stopped = Vec::new();
        for id in resident {
            let name = id.as_str();
            if deployments.kept(name) || current.as_deref() == Some(name) {
                continue;
            }
            let asked = stopping.contains(name);
            let outcome = if asked || due.iter().any(|due| due == name) {
                control.retire_release(name)
            } else {
                control.release_stopped(name)
            };
            match outcome {
                Ok(true) => stopped.push((id.clone(), asked)),
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, deployment = name, "release not yet retired"),
            }
        }
        stopped
    }

    /// While a deployment loads and the backend, read again after this tick's releases, still holds as many as it can,
    /// retires the oldest resident deployment that management no longer asks for, and fences it in the backend once its
    /// JVMs have exited, wherever its jobs and subscriptions stand.
    async fn make_room(&self, control: &Control, backend: &chunk_backend::Backend) {
        let resident = match backend.deployments().await {
            Ok(resident) => resident,
            Err(error) => return tracing::warn!(%error, "resident deployments unknown"),
        };
        if resident.len() < chunk_backend::MAX_DEPLOYMENTS {
            return;
        }
        let oldest = {
            let deployments = lock(&self.deployments);
            let Some(loading) = deployments.loading.as_deref() else { return };
            if resident.iter().any(|id| id.as_str() == loading) {
                return;
            }
            let current = control.current_release().ok().flatten();
            let candidate =
                resident.iter().find(|id| !deployments.kept(id.as_str()) && current.as_deref() != Some(id.as_str()));
            let Some(candidate) = candidate else { return };
            match control.retire_release(candidate.as_str()) {
                Ok(true) => candidate.clone(),
                Ok(false) => return,
                Err(error) => return tracing::warn!(%error, "no deployment retired to make room"),
            }
        };
        tracing::debug!(deployment = oldest.as_str(), "oldest deployment retiring to make room");
        if let Err(error) = backend.retire(oldest.clone()).await {
            tracing::warn!(%error, deployment = oldest.as_str(), "backend version not retired");
        }
    }

    /// Removes the unpacked releases and their archives that neither control, whose JVMs run from them, nor a load
    /// claims.
    async fn remove_unused_releases(&self) {
        let Ok(control) = self.core.control() else { return };
        let used = match control.release_ids() {
            Ok(used) => used,
            Err(error) => return tracing::warn!(%error, "releases in use unknown"),
        };
        // Set aside without an await in between, so no loaded release activates and drops its claim meanwhile.
        let unused = super::release::set_aside(&self.releases, used);
        if let Err(error) = super::release::remove(unused).await {
            tracing::warn!(%error, "unused releases not removed");
        }
    }
}
