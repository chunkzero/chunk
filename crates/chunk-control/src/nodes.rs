use crate::{
    Control, Error, Result,
    client::{auth, channel},
    drain::retire_host,
};
use chunk_proto::v1::{
    NodeList, NodePhase, NodeStatus, ProcessHealth, ShutdownNodeRequest, node_control_client::NodeControlClient,
};
use prost::Message;
use std::{collections::BTreeSet, sync::Arc, time::Duration};

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
                    let phase = if self.host.stopped(id) {
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
    pub(crate) async fn poll_health(self: &Arc<Self>) -> Result<()> {
        let mut tasks = tokio::task::JoinSet::new();
        for id in self.state()?.hosts.keys().filter(|id| !self.host.stopped(id)) {
            let Some(connection) = self.host.connection(id) else {
                continue;
            };
            let control = self.clone();
            let id = id.clone();
            tasks.spawn(async move {
                let probe = async {
                    let health = NodeControlClient::new(channel(&connection).await?)
                        .health(auth(&connection, connection.identity.clone(), 2)?)
                        .await?
                        .into_inner();
                    if health.identity.as_ref() != Some(&connection.identity) {
                        return Err(Error::Invalid("health identity mismatch"));
                    }
                    Ok(health)
                };
                let health = tokio::time::timeout(Duration::from_secs(3), probe).await.ok().and_then(Result::ok);
                let terminate = {
                    let mut observations =
                        control.observations.lock().map_err(|_| Error::Unresolved("health observations poisoned"))?;
                    let previous = observations.get(&id);
                    let phase = match &health {
                        None => NodePhase::Unreachable,
                        Some(h) if h.draining => NodePhase::Draining,
                        Some(h)
                            if !h.ready
                                || h.tick_count == 0
                                || h.last_tick_age_millis > 5000
                                || previous
                                    .and_then(|o| o.health.as_ref())
                                    .is_some_and(|p| h.tick_count <= p.tick_count) =>
                        {
                            NodePhase::Unhealthy
                        }
                        Some(_) => NodePhase::Online,
                    };
                    let failures = if matches!(phase, NodePhase::Unreachable | NodePhase::Unhealthy) {
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
                    control.shutdown_node(&ShutdownNodeRequest {
                        operation_id: format!("health/{id}"),
                        host_id: id,
                        timeout_seconds: 0,
                    })?;
                }
                Ok::<_, Error>(())
            });
        }
        while let Some(result) = tasks.join_next().await {
            result.map_err(|_| Error::Unresolved("health task failed"))??;
        }
        Ok(())
    }
}
