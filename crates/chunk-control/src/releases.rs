//! The releases one control runs side by side. New sessions are placed on the current release only: a login routed
//! with another one is routed again, and a move out of another one goes to the current release. Recovery uses the
//! release of the host a row runs on. A release is forgotten once none of its hosts remain and every launch that may
//! still run a JVM has a host row naming its release.

use std::{collections::BTreeSet, sync::Arc};

use crate::{
    Control, Error, Release, Result,
    state::{Capacity, ReleaseState, State},
};

/// The launches whose release control may not know, read before the state they are checked against.
pub(crate) struct Launches {
    recovered: bool,
    unowned: BTreeSet<String>,
}

impl Launches {
    /// Whether `state`'s host rows name the release of every launch that may still run a JVM. While recovery is
    /// pending, a surviving JVM may belong to any release.
    pub fn attributed(&self, state: &State) -> bool {
        self.recovered && self.unowned.iter().all(|id| state.hosts.contains_key(id))
    }
}

impl Control {
    /// Records `release` if it is new, and makes it current, ending its drain. Sessions of earlier releases keep running.
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
                    let release = ReleaseState { release: Arc::new(release), retired: false, drain: None };
                    state.releases.insert(name.clone(), release);
                }
            }
            if let Some(release) = state.releases.get_mut(&name) {
                release.drain = None;
            }
            state.current = Some(name);
            Ok(())
        })
    }

    /// Retires `deployment`'s release, which places nothing new from then on, and stops each of its hosts at once,
    /// disconnecting their players. Returns whether every one of its hosts has stopped, as for an unknown release, and
    /// no launch of an unknown release may still run.
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
            release.drain = None;
            let hosts: Vec<_> =
                state.hosts.iter().filter(|(_, host)| host.release == deployment).map(|(id, _)| id.clone()).collect();
            for id in hosts {
                crate::capacity::stop(state, &id, None);
            }
            Ok(())
        })?;
        self.wake_capacity();
        self.release_stopped(deployment)
    }

    /// Whether `deployment`'s release retired and every one of its hosts has stopped, as for an unknown release, and no
    /// launch of an unknown release may still run.
    pub(crate) fn release_stopped(&self, deployment: &str) -> Result<bool> {
        let launches = self.launches()?;
        let state = self.state()?;
        Ok(launches.attributed(&state)
            && state.releases.get(deployment).is_none_or(|release| release.retired)
            && !state.hosts.values().any(|host| host.release == deployment && host.capacity != Capacity::Released))
    }

    /// The IDs of the releases control still knows, retired or not. It forgets a release only once none
    /// of its hosts remain and every launch that may still run a JVM has a host row naming its release.
    /// # Errors
    /// Reports a stopped store.
    pub fn release_ids(&self) -> Result<BTreeSet<String>> {
        Ok(self.state()?.releases.values().map(|release| release.release.release_id.clone()).collect())
    }

    /// The deployment of the current release, where new players are placed.
    /// # Errors
    /// Reports a stopped store.
    pub fn current_release(&self) -> Result<Option<String>> {
        Ok(self.state()?.current.clone())
    }

    pub(crate) fn launches(&self) -> Result<Launches> {
        let unowned = self.host.unowned()?;
        Ok(Launches { recovered: self.recovery.open()?, unowned })
    }
}
