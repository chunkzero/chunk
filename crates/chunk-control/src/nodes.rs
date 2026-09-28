use crate::{
    Control, Error, Result,
    drain::retire_host,
    state::{Capacity, State},
};
use chunk_proto::{
    control::v1::ShutdownNodeRequest,
    sync::v1::{JvmHealth, NodePhase},
};
use prost::Message;
use std::collections::BTreeSet;

/// A host's lifecycle, and the health its JVM last reported.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeStatus {
    pub host: String,
    /// The deployment whose release the host runs.
    pub deployment: String,
    pub app: String,
    pub profile: String,
    pub phase: NodePhase,
    /// Unset before the JVM's first report.
    pub health: Option<JvmHealth>,
    /// Unix time in milliseconds of the last health sample; zero before the first.
    pub observed_at_ms: u64,
    /// Health checks failed in a row.
    pub consecutive_failures: u32,
}

pub(crate) struct Observation {
    phase: NodePhase,
    health: Option<JvmHealth>,
    at: u64,
    failures: u32,
}
impl Control {
    /// Current lifecycle and last observed metrics; an unreachable process still owns its players.
    /// # Errors
    /// Reports unavailable durable state.
    pub fn nodes(&self) -> Result<Vec<NodeStatus>> {
        let state = self.state()?;
        self.statuses(&state)
    }

    /// Each host's lifecycle in `state`, with its last observed metrics.
    pub(crate) fn statuses(&self, state: &State) -> Result<Vec<NodeStatus>> {
        let observations = self.observations.lock().map_err(|_| Error::Unresolved("health observations poisoned"))?;
        Ok(state
            .hosts
            .iter()
            .map(|(id, host)| {
                let observation = observations.get(id);
                let phase = if host.capacity == Capacity::Released {
                    NodePhase::Stopped
                } else if self.host.unresolved(id) {
                    NodePhase::Unreachable
                } else if host.retired {
                    if drain_deadline(state, id).is_some_and(|deadline| deadline > crate::now_ms()) {
                        NodePhase::Draining
                    } else {
                        NodePhase::Stopping
                    }
                } else {
                    observation.map_or(NodePhase::Starting, |o| o.phase)
                };
                NodeStatus {
                    host: id.clone(),
                    deployment: host.release.clone(),
                    app: host.app.clone(),
                    profile: host.profile.clone(),
                    phase,
                    health: observation.and_then(|o| o.health),
                    observed_at_ms: observation.map_or(0, |o| o.at),
                    consecutive_failures: observation.map_or(0, |o| o.failures),
                }
            })
            .collect())
    }

    /// Retires capacity immediately and queues evacuation with a bounded shutdown deadline.
    /// # Errors
    /// Rejects changed operations, unknown nodes, or invalid deadlines.
    pub fn shutdown_node(&self, request: &ShutdownNodeRequest) -> Result<()> {
        if request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || !(0..=120).contains(&request.timeout_seconds)
        {
            return Err(Error::Invalid("invalid node shutdown"));
        }
        let operation = format!("node/{}", request.operation_id);
        self.update(|state| {
            retire_host(state, operation, request.encode_to_vec(), request.timeout_seconds, false, |state| {
                state
                    .hosts
                    .contains_key(&request.host_id)
                    .then(|| request.host_id.clone())
                    .ok_or(Error::Invalid("unknown node"))
            })
        })
    }
    pub(crate) fn unavailable(&self) -> Result<BTreeSet<String>> {
        Ok(self
            .observations
            .lock()
            .map_err(|_| Error::Unresolved("health observations poisoned"))?
            .iter()
            .filter(|(_, o)| o.phase != NodePhase::Online)
            .map(|(id, _)| id.clone())
            .collect())
    }
    /// Records each running JVM's health from what it last pushed in its reports, and retires a host after three
    /// unhealthy passes in a row, or once its JVM reports it is draining. Announces the pass even when a retirement
    /// fails, since its observations changed.
    pub(crate) fn poll_health(&self) -> Result<()> {
        let polled = self.observe_health();
        self.observed.send_modify(|passes| *passes += 1);
        polled
    }

    fn observe_health(&self) -> Result<()> {
        let state = self.state()?;
        for id in state.hosts.keys().filter(|id| !state.released(id)) {
            if self.host.connection(id).is_none() {
                continue;
            }
            let health = self.jvms.health(id);
            let terminate = {
                let mut observations =
                    self.observations.lock().map_err(|_| Error::Unresolved("health observations poisoned"))?;
                let previous = observations.get(id);
                let phase = match &health {
                    Some(h) if h.draining => NodePhase::Draining,
                    Some(h) if h.ready && h.tick_count > 0 && h.last_tick_age_millis <= 5000 => NodePhase::Online,
                    _ => NodePhase::Unhealthy,
                };
                let failures = if phase == NodePhase::Unhealthy {
                    previous.map_or(1, |o| o.failures.saturating_add(1))
                } else {
                    0
                };
                let at = if health.is_some() { crate::now_ms() } else { previous.map_or(0, |o| o.at) };
                let retained = health.or_else(|| previous.and_then(|o| o.health));
                observations.insert(id.clone(), Observation { phase, health: retained, at, failures });
                failures >= 3 || phase == NodePhase::Draining
            };
            if terminate {
                self.shutdown_node(&ShutdownNodeRequest {
                    operation_id: format!("health/{id}"),
                    host_id: id.clone(),
                    timeout_seconds: 0,
                })?;
            }
        }
        Ok(())
    }

    /// Follows health passes, which change observations without committing.
    pub(crate) fn observed(&self) -> tokio::sync::watch::Receiver<u64> {
        self.observed.subscribe()
    }
}

/// The earliest deadline of `host`'s drains, when core stops it.
pub(crate) fn drain_deadline(state: &State, host: &str) -> Option<u64> {
    state.drains.values().filter(|drain| drain.host == host).map(|drain| drain.deadline_ms).min()
}
