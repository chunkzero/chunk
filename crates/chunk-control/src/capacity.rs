//! Host capacity intents. Placement commits the capacity it needs and drains commit the capacity they release before
//! any host call, and this executor makes each host's calls, keyed by the host ID, committing their outcomes back to
//! the log. After a restart it resumes every unfinished intent from the log.

use std::{collections::HashMap, sync::Arc, time::Duration};

use tokio::{sync::Semaphore, task::JoinSet};
use tokio_util::sync::CancellationToken;

use crate::{
    Control, Result,
    host::Progress,
    state::{Capacity, HostState, Phase, State},
};

impl Control {
    /// Advances host capacity intents until `stop`, then cancels and awaits the calls in flight. Runs a pass on each
    /// [`Control::wake_capacity`] and every second, with at most one call per host at a time.
    pub(crate) async fn run_capacity(self: &Arc<Self>, stop: &CancellationToken) {
        let permits = Arc::new(Semaphore::new(8));
        let mut calls = JoinSet::new();
        let mut hosts = HashMap::new();
        let mut timer = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                () = stop.cancelled() => break,
                Some(joined) = calls.join_next_with_id() => {
                    hosts.remove(&joined.map_or_else(|error| error.id(), |(task, ())| task));
                    continue;
                }
                () = self.capacity.notified() => {}
                _ = timer.tick() => {}
            }
            let Ok(state) = self.state() else {
                continue;
            };
            for (id, _) in state.hosts.iter().filter(|(_, host)| called(host)) {
                if hosts.values().any(|host| host == id) {
                    continue;
                }
                let (control, permits, host) = (self.clone(), permits.clone(), id.clone());
                let call = calls.spawn(async move {
                    let Ok(_permit) = permits.acquire_owned().await else {
                        return;
                    };
                    if let Err(error) = control.advance_capacity(&host).await {
                        tracing::debug!(%error, host, "host capacity call will be retried");
                    }
                });
                hosts.insert(call.id(), id.clone());
            }
        }
        calls.shutdown().await;
    }

    /// Runs a capacity pass soon, after a commit that requested or released capacity or a runtime registration.
    pub(crate) fn wake_capacity(&self) {
        self.capacity.notify_one();
    }

    /// Makes the next host call for `id`'s capacity intent and commits its outcome.
    async fn advance_capacity(&self, id: &str) -> Result<()> {
        let state = self.state()?;
        let Some(host) = state.hosts.get(id).filter(|host| called(host)) else {
            return Ok(());
        };
        match host.capacity {
            // An exit nothing asked for; only the host's affirmative evidence releases ready capacity.
            Capacity::Ready if self.host.stopped(id) => self.update(|state| released(state, id, Capacity::Ready)),
            // Retired capacity that never became ready is released without starting it.
            from @ (Capacity::Releasing | Capacity::Requested) if from == Capacity::Releasing || host.retired => {
                if !self.host.release(id).await? {
                    return Ok(());
                }
                self.update(|state| released(state, id, from))
            }
            // A retired host never starts again under its ID; its drain releases it.
            _ if host.retired => Ok(()),
            _ => self.ensure(id, host).await,
        }
    }

    async fn ensure(&self, id: &str, host: &HostState) -> Result<()> {
        let failure = match self.host.ensure(id, &host.app, &host.profile).await? {
            Progress::Pending => return Ok(()),
            Progress::Ready(runtime) if self.runs_host(&runtime, host) => None,
            Progress::Ready(_) => Some("host returned incompatible runtime".to_owned()),
            Progress::Failed(reason) => Some(reason),
        };
        let Some(reason) = failure else {
            return self.update(|state| {
                if let Some(host) = state.hosts.get_mut(id).filter(|host| host.capacity == Capacity::Requested) {
                    host.capacity = Capacity::Ready;
                }
                Ok(())
            });
        };
        tracing::warn!(host = id, reason, "host cannot provide its capacity; releasing it");
        self.update(|state| {
            fail(state, id, reason);
            Ok(())
        })?;
        self.wake_capacity();
        Ok(())
    }
}

/// Whether `host`'s capacity intent needs a host call.
fn called(host: &HostState) -> bool {
    host.capacity != Capacity::Released
}

/// Records that `id`'s runtime exited while its capacity was `from`: retires the host, finishes its sessions and
/// releases the claims prepared on them.
fn released(state: &mut State, id: &str, from: Capacity) -> Result<()> {
    let Some(host) = state.hosts.get_mut(id).filter(|host| host.capacity == from) else {
        return Ok(());
    };
    host.capacity = Capacity::Released;
    host.retired = true;
    for session in state.sessions.values_mut().filter(|session| session.host == id) {
        session.retired = true;
        session.finished = true;
    }
    let prepared: Vec<_> = state
        .claims
        .iter()
        .filter(|(_, claim)| claim.phase != Phase::Released && claim.assignment.is_some())
        .filter(|(_, claim)| state.sessions.get(&claim.session).is_some_and(|session| session.host == id))
        .map(|(operation, _)| operation.clone())
        .collect();
    for operation in prepared {
        crate::delivery::release(state, &operation)?;
    }
    Ok(())
}

/// Releases `id`'s capacity after its host failed to provide it, retiring the host and its sessions.
fn fail(state: &mut State, id: &str, reason: String) {
    let Some(host) = state.hosts.get_mut(id).filter(|host| host.capacity < Capacity::Releasing) else {
        return;
    };
    host.capacity = Capacity::Releasing;
    host.failure = Some(reason);
    host.retired = true;
    for session in state.sessions.values_mut().filter(|session| session.host == id) {
        session.retired = true;
    }
}
