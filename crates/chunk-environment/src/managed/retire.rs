//! Retiring what the environment no longer serves, derived each time from the backend's and control's durable state so
//! that a restart at any point converges.

use super::{Managed, lock, release};
use chunk_control::{Control, DrainPolicy};
use chunk_management::v1;
use std::{convert::Infallible, time::Duration};

/// How the deployments the desired one replaces retire.
#[derive(Default)]
pub(super) struct Retiring {
    drain: DrainPolicy,
    /// Stop them at once, once the desired deployment is current.
    stop_previous: bool,
}

impl From<&v1::AttachResponse> for Retiring {
    /// Without drain settings, replaced deployments stop at once.
    fn from(desired: &v1::AttachResponse) -> Self {
        let settings = desired.drain.unwrap_or_default();
        let seconds = |seconds: u32| Some(Duration::from_secs(seconds.into()));
        let drain =
            DrainPolicy { max_age: seconds(settings.max_age_seconds), deadline: seconds(settings.deadline_seconds) };
        Self { drain, stop_previous: desired.stop_previous }
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

    /// Drains every deployment the backend holds that is neither control's current one nor kept for management, or stops
    /// it at once when the desired deployment asked so, then releases its backend version once its JVMs have all exited.
    /// A deployment loading while the backend holds as many as it can retires the longest-draining one to make room.
    async fn retire(&self) {
        let (Ok(control), Some(backend)) = (self.core.control(), self.core.backend()) else { return };
        let resident = match backend.deployments().await {
            Ok(resident) => resident,
            Err(error) => return tracing::warn!(%error, "resident deployments unknown"),
        };
        let resident_names: Vec<_> = resident.iter().map(|id| id.as_str().to_owned()).collect();
        let loading = lock(&self.deployments).loading.clone();
        if loading.is_some_and(|loading| !resident_names.contains(&loading))
            && resident.len() >= chunk_backend::MAX_DEPLOYMENTS
        {
            self.make_room(&control, &resident_names);
        }
        for id in resident {
            let deployment = id.as_str();
            // Checked and retired without an await in between, so management cannot ask for it meanwhile.
            let stopped = {
                let deployments = lock(&self.deployments);
                if deployments.kept(deployment) {
                    continue;
                }
                match control.current_release() {
                    Ok(current) if current.as_deref() == Some(deployment) => continue,
                    Ok(current) if deployments.retiring.stop_previous && current == deployments.desired => {
                        control.retire_release(deployment)
                    }
                    Ok(_) => control.drain_release(deployment, deployments.retiring.drain),
                    Err(error) => Err(error),
                }
            };
            match stopped {
                Ok(true) => match backend.release(id.clone()).await {
                    Ok(_) | Err(chunk_backend::Error::Busy) => {}
                    Err(error) => tracing::warn!(%error, deployment, "backend version not released"),
                },
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, deployment, "release not yet retired"),
            }
        }
    }

    /// Stops the longest-draining resident deployment that management no longer asks for, unless one is stopping
    /// already.
    fn make_room(&self, control: &Control, resident: &[String]) {
        let candidates: Vec<_> = {
            let deployments = lock(&self.deployments);
            resident.iter().filter(|name| !deployments.kept(name)).cloned().collect()
        };
        match control.retire_longest_draining(&candidates) {
            Ok(Some(oldest)) => tracing::info!(deployment = oldest, "longest-draining deployment stopped to make room"),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "no draining deployment stopped to make room"),
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
        let unused = release::set_aside(&self.releases, used);
        if let Err(error) = release::remove(unused).await {
            tracing::warn!(%error, "unused releases not removed");
        }
    }
}
