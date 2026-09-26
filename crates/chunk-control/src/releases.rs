//! The releases one control runs side by side. New logins are placed on the current release; every other placement,
//! and recovery, uses the release of the host a row runs on. A release is forgotten once none of its hosts remain.

use std::sync::Arc;

use crate::{
    Control, Error, Release, Result,
    state::{Capacity, ReleaseState},
};

impl Control {
    /// Records `release` if it is new, and makes it current. Sessions of earlier releases keep running.
    /// # Errors
    /// Rejects an invalid release, one of another environment, a changed or retired release, and a stopped store.
    pub fn activate_release(&self, release: Release) -> Result<()> {
        release.validate()?;
        if release.deployment.environment != self.config.environment {
            return Err(Error::Invalid("release belongs to another environment"));
        }
        let name = release.deployment.deployment.clone();
        let recorded = serde_json::to_vec(&release)?;
        self.update(|state| {
            match state.releases.get(&name) {
                Some(existing) if existing.retired => return Err(Error::Invalid("release retired")),
                Some(existing) if serde_json::to_vec(&*existing.release)? != recorded => {
                    return Err(Error::Invalid("release configuration changed"));
                }
                Some(_) => {}
                None => {
                    state.releases.insert(name.clone(), ReleaseState { release: Arc::new(release), retired: false });
                }
            }
            state.current = Some(name);
            Ok(())
        })
    }

    /// Retires `deployment`'s release, which places nothing new from then on, and stops each of its hosts at once,
    /// disconnecting their players. Returns whether every one of its hosts has stopped, as for an unknown release.
    /// # Errors
    /// Rejects the current release and reports a stopped store.
    pub fn retire_release(&self, deployment: &str) -> Result<bool> {
        self.update(|state| {
            if state.current.as_deref() == Some(deployment) {
                return Err(Error::Invalid("the current release cannot retire"));
            }
            let Some(release) = state.releases.get_mut(deployment) else {
                return Ok(());
            };
            release.retired = true;
            let hosts: Vec<_> =
                state.hosts.iter().filter(|(_, host)| host.release == deployment).map(|(id, _)| id.clone()).collect();
            for id in hosts {
                crate::capacity::stop(state, &id, None);
            }
            Ok(())
        })?;
        self.wake_capacity();
        let state = self.state()?;
        Ok(!state.hosts.values().any(|host| host.release == deployment && host.capacity != Capacity::Released))
    }
}
