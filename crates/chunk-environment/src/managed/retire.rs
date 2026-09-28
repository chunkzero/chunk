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

    /// Stops the JVMs of every deployment the backend holds that is neither control's current one nor kept for
    /// management, then releases its backend version once they have all exited.
    async fn retire(&self) {
        let (Ok(control), Some(backend)) = (self.core.control(), self.core.backend()) else { return };
        let resident = match backend.deployments().await {
            Ok(resident) => resident,
            Err(error) => return tracing::warn!(%error, "resident deployments unknown"),
        };
        for id in resident {
            let deployment = id.as_str();
            // Checked and retired without an await in between, so management cannot ask for it meanwhile.
            let stopped = {
                if lock(&self.deployments).kept(deployment) {
                    continue;
                }
                match control.current_release() {
                    Ok(current) if current.as_deref() == Some(deployment) => continue,
                    Ok(_) => control.retire_release(deployment),
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
