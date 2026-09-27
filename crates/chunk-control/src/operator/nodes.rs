//! The `nodes` topic: each host's lifecycle and health, keyed by host ID, with `chunk.sync.v1.Node` values. Health and
//! a passing drain deadline change it without a commit, so it follows health passes and deadlines as well as commits.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use chunk_proto::{
    sync::v1 as sync,
    v1::{NodePhase, NodeStatus, ProcessHealth},
};
use tokio::sync::watch;

use super::{changed, entry};
use crate::{
    Control, Generation, Result,
    gateway::position,
    nodes::drain_deadline,
    state::{Phase, State},
};

/// One stream of the `nodes` topic.
pub struct Nodes {
    control: Arc<Control>,
    positions: watch::Receiver<Generation>,
    observed: watch::Receiver<u64>,
    /// The value last sent for each host.
    sent: BTreeMap<String, sync::Node>,
    /// The position of the last update.
    position: Generation,
    /// The earliest deadline of a draining host, when it starts stopping.
    wake: Option<u64>,
}

impl Nodes {
    /// Opens a stream and returns its first update, a snapshot.
    /// # Errors
    /// Reports unreadable control state.
    pub fn open(control: &Arc<Control>) -> Result<(Self, sync::Update)> {
        let mut nodes = Self {
            control: control.clone(),
            positions: control.subscribe(),
            observed: control.observed(),
            sent: BTreeMap::new(),
            position: Generation::default(),
            wake: None,
        };
        let first = nodes.diff(true)?.unwrap_or_default();
        Ok((nodes, first))
    }

    /// Resolves once control committed, a health pass ran or a draining host's deadline passed.
    pub async fn changed(&mut self) {
        let wake = self.wake.map(|deadline| Duration::from_millis(deadline.saturating_sub(crate::now_ms())));
        tokio::select! {
            () = changed(&mut self.positions) => {}
            () = changed(&mut self.observed) => {}
            () = async {
                match wake {
                    Some(wake) => tokio::time::sleep(wake).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }

    /// The hosts that changed since the previous update, or `None` when none did and control has not committed.
    /// # Errors
    /// Reports unreadable control state.
    pub fn update(&mut self) -> Result<Option<sync::Update>> {
        self.diff(false)
    }

    fn diff(&mut self, snapshot: bool) -> Result<Option<sync::Update>> {
        let now = crate::now_ms();
        let state = self.control.state()?;
        let nodes = nodes(&state, self.control.statuses(&state)?);
        self.wake = nodes
            .values()
            .filter(|node| node.phase() == sync::NodePhase::Draining)
            .map(|node| node.drain_deadline_ms)
            .filter(|&deadline| deadline > now)
            .min();
        let upserts: Vec<_> = nodes
            .iter()
            .filter(|(id, node)| snapshot || self.sent.get(*id) != Some(node))
            .map(|(id, node)| entry(id.clone(), node))
            .collect();
        let removed: Vec<_> = self.sent.keys().filter(|id| !snapshot && !nodes.contains_key(*id)).cloned().collect();
        if !snapshot && upserts.is_empty() && removed.is_empty() && state.position() == self.position {
            return Ok(None);
        }
        self.sent = nodes;
        self.position = state.position();
        Ok(Some(sync::Update {
            position: position(self.position),
            snapshot,
            upserts,
            removed,
            ..sync::Update::default()
        }))
    }
}

/// Each host's node from its status, with its drain's deadline and open claims while it drains or stops.
fn nodes(state: &State, statuses: Vec<NodeStatus>) -> BTreeMap<String, sync::Node> {
    let retiring = |phase: NodePhase| matches!(phase, NodePhase::Draining | NodePhase::Stopping);
    let hosts: BTreeSet<_> =
        statuses.iter().filter(|status| retiring(status.phase())).map(|status| status.host_id.as_str()).collect();
    let mut remaining = BTreeMap::<&str, u32>::new();
    if !hosts.is_empty() {
        for claim in state.claims.values().filter(|claim| claim.phase != Phase::Released) {
            let host = state.sessions.get(&claim.session).map(|session| session.host.as_str());
            if let Some(host) = host.filter(|host| hosts.contains(host)) {
                *remaining.entry(host).or_default() += 1;
            }
        }
    }
    let mut nodes = BTreeMap::new();
    for status in statuses {
        let phase = status.phase();
        let draining = phase == NodePhase::Draining;
        let node = sync::Node {
            drain_deadline_ms: draining.then(|| drain_deadline(state, &status.host_id)).flatten().unwrap_or_default(),
            remaining_claims: remaining.get(status.host_id.as_str()).copied().filter(|_| retiring(phase)).unwrap_or(0),
            deployment: status.deployment,
            app: status.app_id,
            machine_profile: status.machine_profile,
            phase: node_phase(phase).into(),
            health: status.health.as_ref().map(health),
            observed_at_ms: status.observed_at_ms,
            consecutive_failures: status.consecutive_failures,
        };
        nodes.insert(status.host_id, node);
    }
    nodes
}

fn node_phase(phase: NodePhase) -> sync::NodePhase {
    match phase {
        NodePhase::Unspecified => sync::NodePhase::Unspecified,
        NodePhase::Starting => sync::NodePhase::Starting,
        NodePhase::Online => sync::NodePhase::Online,
        NodePhase::Unhealthy => sync::NodePhase::Unhealthy,
        NodePhase::Unreachable => sync::NodePhase::Unreachable,
        NodePhase::Draining => sync::NodePhase::Draining,
        NodePhase::Stopping => sync::NodePhase::Stopping,
        NodePhase::Stopped => sync::NodePhase::Stopped,
    }
}

fn health(health: &ProcessHealth) -> sync::JvmHealth {
    sync::JvmHealth {
        ready: health.ready,
        draining: health.draining,
        tick_count: health.tick_count,
        last_tick_age_millis: health.last_tick_age_millis,
        heap_used_bytes: health.heap_used_bytes,
        heap_max_bytes: health.heap_max_bytes,
        gc_count: health.gc_count,
        gc_time_millis: health.gc_time_millis,
        process_cpu_load: health.process_cpu_load,
        sessions: health.sessions,
        players: health.players,
    }
}
