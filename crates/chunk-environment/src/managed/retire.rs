//! Retiring what the environment no longer serves, derived each time from the backend's and control's durable state so
//! that a restart at any point converges.

use super::{Managed, lock, release};
use std::{convert::Infallible, time::Duration};

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

    /// Once a deployment serves, stops the JVMs of every other deployment the backend holds, except one being loaded,
    /// then releases its backend version once they have all exited.
    async fn retire(&self) {
        let (Ok(control), Some(backend)) = (self.core.control(), self.core.backend()) else { return };
        let resident = match backend.deployments().await {
            Ok(resident) => resident,
            Err(error) => return tracing::warn!(%error, "resident deployments unknown"),
        };
        for id in resident {
            let deployment = id.as_str();
            // Checked and retired without an await in between, so no deployment starts loading it meanwhile.
            let stopped = {
                let deployments = lock(&self.deployments);
                let serving = deployments.serving.as_deref();
                let loading = deployments.loading.as_ref().map(|(loading, _)| loading.as_str());
                if serving.is_none_or(|serving| serving == deployment) || loading == Some(deployment) {
                    continue;
                }
                control.retire_release(deployment)
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

    /// Removes the unpacked releases that neither control, whose JVMs run from them, nor a loading deployment uses.
    async fn remove_unused_releases(&self) {
        let Ok(control) = self.core.control() else { return };
        let mut used = match control.release_artifacts() {
            Ok(used) => used,
            Err(error) => return tracing::warn!(%error, "releases in use unknown"),
        };
        // Set aside without an await in between, so no deployment starts loading one meanwhile.
        let unused = {
            let deployments = lock(&self.deployments);
            used.extend(deployments.loading.as_ref().map(|(_, release)| release.clone()));
            release::set_aside(&self.releases, &used)
        };
        if let Err(error) = release::remove(unused).await {
            tracing::warn!(%error, "unused releases not removed");
        }
    }
}
