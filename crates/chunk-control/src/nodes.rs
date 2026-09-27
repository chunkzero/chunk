use crate::{Control, Error, Result, drain::retire_host, state::Capacity};
use chunk_proto::v1::{NodeList, NodePhase, NodeStatus, ProcessHealth, ShutdownNodeRequest};
use prost::Message;
use std::collections::BTreeSet;

pub(crate) struct Observation {
    phase: NodePhase,
    health: Option<ProcessHealth>,
    at: u64,
    failures: u32,
}
impl Control {
    /// Current lifecycle and last observed metrics; an unreachable process still owns its players.
    /// # Errors
    /// Reports unavailable durable state.
    pub fn nodes(&self) -> Result<NodeList> {
        let state = self.state()?;
        let observations = self.observations.lock().map_err(|_| Error::Unresolved("health observations poisoned"))?;
        Ok(NodeList {
            nodes: state
                .hosts
                .iter()
                .map(|(id, host)| {
                    let observation = observations.get(id);
                    let phase = if host.capacity == Capacity::Released {
                        NodePhase::Stopped
                    } else if self.host.unresolved(id) {
                        NodePhase::Unreachable
                    } else if host.retired {
                        if state
                            .drains
                            .values()
                            .filter(|d| d.host == *id)
                            .map(|d| d.deadline_ms)
                            .min()
                            .is_some_and(|deadline| deadline > crate::now_ms())
                        {
                            NodePhase::Draining
                        } else {
                            NodePhase::Stopping
                        }
                    } else {
                        observation.map_or(NodePhase::Starting, |o| o.phase)
                    };
                    NodeStatus {
                        host_id: id.clone(),
                        deployment: host.release.clone(),
                        app_id: host.app.clone(),
                        machine_profile: host.profile.clone(),
                        phase: phase.into(),
                        health: observation.and_then(|o| o.health.clone()),
                        observed_at_ms: observation.map_or(0, |o| o.at),
                        consecutive_failures: observation.map_or(0, |o| o.failures),
                    }
                })
                .collect(),
        })
    }
    /// Retires capacity immediately and queues evacuation with a bounded shutdown deadline.
    /// # Errors
    /// Rejects changed operations, unknown nodes, or invalid deadlines.
    pub fn shutdown_node(&self, request: &ShutdownNodeRequest) -> Result<NodeStatus> {
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
        })?;
        self.nodes()?.nodes.into_iter().find(|n| n.host_id == request.host_id).ok_or(Error::Invalid("unknown node"))
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
    /// unhealthy passes in a row, or once its JVM reports it is draining.
    pub(crate) fn poll_health(&self) -> Result<()> {
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
                let retained = health.or_else(|| previous.and_then(|o| o.health.clone()));
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
}
