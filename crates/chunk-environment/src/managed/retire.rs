//! Retiring what the environment no longer serves, derived each time from the backend's and control's durable state so
//! that a restart at any point converges. Control starts a replaced deployment's drain at its activation.

use super::{
    Managed,
    activation::{Records, Stopping},
    lock,
};
use chunk_control::{Control, DrainPolicy};
use chunk_management::v1;
use std::{collections::BTreeSet, convert::Infallible, io, time::Duration};

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

/// The deployments that stop at once once `activated` is current: those that already did and, when `stop_previous`,
/// every other resident one.
pub(super) fn stopping_after(
    stopping: &BTreeSet<String>,
    resident: &[chunk_js::DeploymentId],
    activated: &str,
    stop_previous: bool,
) -> BTreeSet<String> {
    let mut stopping = stopping.clone();
    if stop_previous {
        stopping.extend(resident.iter().map(|id| id.as_str().to_owned()));
    }
    stopping.remove(activated);
    stopping
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

    /// Records `stopping` as of control's `current` deployment, durably.
    pub(super) fn record_stopping(&self, current: Option<&str>, stopping: &BTreeSet<String>) -> io::Result<()> {
        let stops = current.map(|current| Stopping { current: current.to_owned(), deployments: stopping.clone() });
        Records { committed: stops.as_ref(), pending: None }.store(&self.stop_record)
    }

    /// Retires every deployment the backend holds that is neither control's current one nor kept for management and
    /// that is due or was asked to stop, then releases the backend version of each whose JVMs have all exited. A
    /// deployment loading while the backend holds as many as it can retires the oldest one to make room.
    async fn retire(&self) {
        let (Ok(control), Some(backend)) = (self.core.control(), self.core.backend()) else { return };
        let resident = match backend.deployments().await {
            Ok(resident) => resident,
            Err(error) => return tracing::warn!(%error, "resident deployments unknown"),
        };
        let stopped = self.retire_resident(&control, &resident);
        for id in &stopped {
            match backend.release(id.clone()).await {
                Ok(_) | Err(chunk_backend::Error::Busy) => {}
                Err(error) => tracing::warn!(%error, deployment = id.as_str(), "backend version not released"),
            }
        }
        self.make_room(&control, &backend, &resident).await;
    }

    /// Retires the resident deployments that are due or asked to stop, and returns those whose JVMs have all exited.
    /// Checked and retired without an await in between, so management cannot ask for one meanwhile.
    fn retire_resident(&self, control: &Control, resident: &[chunk_js::DeploymentId]) -> Vec<chunk_js::DeploymentId> {
        let mut deployments = lock(&self.deployments);
        let names: BTreeSet<_> = resident.iter().map(|id| id.as_str().to_owned()).collect();
        let current = match control.current_release() {
            Ok(current) => current,
            Err(error) => {
                tracing::warn!(%error, "current release unknown");
                return Vec::new();
            }
        };
        if deployments.stopping.iter().any(|name| !names.contains(name)) {
            deployments.stopping.retain(|name| names.contains(name));
            if let Err(error) = self.record_stopping(current.as_deref(), &deployments.stopping) {
                tracing::warn!(%error, "deployments to stop not recorded");
            }
        }
        let due = control.due_releases().unwrap_or_default();
        let mut stopped = Vec::new();
        for id in resident {
            let name = id.as_str();
            if deployments.kept(name) || current.as_deref() == Some(name) {
                continue;
            }
            let outcome = if deployments.stopping.contains(name) || due.iter().any(|due| due == name) {
                control.retire_release(name)
            } else {
                control.release_stopped(name)
            };
            match outcome {
                Ok(true) => stopped.push(id.clone()),
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, deployment = name, "release not yet retired"),
            }
        }
        stopped
    }

    /// While a deployment loads and the backend holds as many as it can, retires the oldest resident deployment that
    /// management no longer asks for, and fences it in the backend once its JVMs have exited, wherever its jobs and
    /// subscriptions stand.
    async fn make_room(
        &self,
        control: &Control,
        backend: &chunk_backend::Backend,
        resident: &[chunk_js::DeploymentId],
    ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn resident(names: &[&str]) -> Vec<chunk_js::DeploymentId> {
        names.iter().map(|name| chunk_js::DeploymentId::new(*name).unwrap()).collect()
    }

    #[test]
    fn a_stop_previous_activation_keeps_stopping_every_predecessor_as_later_deployments_activate() {
        let none = BTreeSet::new();
        // A drains, B is current, C replaces it asking to stop what it replaces, and ordinary D follows.
        let after_b = stopping_after(&none, &resident(&["a", "b"]), "b", false);
        assert!(after_b.is_empty());
        let after_c = stopping_after(&after_b, &resident(&["a", "b", "c"]), "c", true);
        assert_eq!(after_c, BTreeSet::from(["a".to_owned(), "b".to_owned()]));
        let after_d = stopping_after(&after_c, &resident(&["a", "b", "c", "d"]), "d", false);
        assert_eq!(after_d, after_c);
        // Management rolls back to a deployment that was to stop, which becomes current again.
        assert!(!stopping_after(&after_d, &resident(&["b", "c", "d"]), "b", false).contains("b"));
    }
}
