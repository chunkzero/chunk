//! The `nodes` topic: each host's lifecycle, health and open sessions, keyed by host ID, with `chunk.sync.v1.Node`
//! values. Health and a passing drain deadline change it without a commit, so it follows health passes and deadlines
//! as well as commits. A session's phase comes from its JVM's last report, read whenever the topic recomputes.

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use chunk_proto::sync::v1::{self as sync, JvmSessionPhase, NodePhase};
use tokio::sync::watch;

use super::{changed, entry};
use crate::{
    Control, Generation, NodeStatus, Result,
    gateway::position,
    nodes::drain_deadline,
    state::{Phase, State},
};

/// One stream of the `nodes` topic.
pub struct Nodes {
    control: Arc<Control>,
    positions: watch::Receiver<Generation>,
    observed: watch::Receiver<u64>,
    /// Open claims by session, as of `position`.
    claims: OpenClaims,
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
            claims: OpenClaims::default(),
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
        // Only the claims committed since the last update are re-read, unless they fell outside retained history.
        let state =
            if let Some((changes, state)) = self.control.authority.feed().after(self.position).filter(|_| !snapshot) {
                for change in &changes {
                    self.claims.track(&state, &change.claim);
                }
                state
            } else {
                let state = self.control.state()?;
                self.claims = OpenClaims::read(&state);
                state
            };
        let nodes = nodes(&self.control, &state, &self.claims, self.control.statuses(&state)?);
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

/// Each open claim's session and each session's count of them.
#[derive(Default)]
struct OpenClaims {
    sessions: BTreeMap<String, String>,
    counts: BTreeMap<String, u32>,
}

impl OpenClaims {
    fn read(state: &State) -> Self {
        let mut claims = Self::default();
        for id in state.claims.keys() {
            claims.track(state, id);
        }
        claims
    }

    /// Re-reads `claim` from `state`.
    fn track(&mut self, state: &State, claim: &str) {
        let session =
            state.claims.get(claim).filter(|claim| claim.phase != Phase::Released).map(|claim| claim.session.clone());
        let previous = match &session {
            Some(session) => self.sessions.insert(claim.to_owned(), session.clone()),
            None => self.sessions.remove(claim),
        };
        if previous == session {
            return;
        }
        if let Some(previous) = previous
            && let Some(count) = self.counts.get_mut(&previous)
        {
            *count -= 1;
            if *count == 0 {
                self.counts.remove(&previous);
            }
        }
        if let Some(session) = session {
            *self.counts.entry(session).or_default() += 1;
        }
    }

    fn count(&self, session: &str) -> u32 {
        self.counts.get(session).copied().unwrap_or(0)
    }
}

/// Each host's node from its status, with its open sessions, drain deadline and open claims while it drains or stops.
fn nodes(
    control: &Control,
    state: &State,
    claims: &OpenClaims,
    statuses: Vec<NodeStatus>,
) -> BTreeMap<String, sync::Node> {
    let retiring = |phase: NodePhase| matches!(phase, NodePhase::Draining | NodePhase::Stopping);
    let mut remaining = BTreeMap::<&str, u32>::new();
    let mut identities = BTreeMap::new();
    let mut sessions = BTreeMap::<&str, Vec<sync::NodeSession>>::new();
    for (id, session) in &state.sessions {
        let players = claims.count(id);
        *remaining.entry(session.host.as_str()).or_default() += players;
        if session.finished {
            continue;
        }
        let identity = identities
            .entry(session.host.as_str())
            .or_insert_with(|| control.host.connection(&session.host).map(|runtime| runtime.identity));
        let reported = identity
            .as_ref()
            .and_then(|identity| control.links.session(&session.host, identity, id))
            .filter(|observed| crate::sessions::matches(id, session, observed))
            .map(|observed| observed.phase());
        let phase = match reported {
            Some(phase @ (JvmSessionPhase::Ending | JvmSessionPhase::Ended | JvmSessionPhase::Failed)) => phase,
            _ if session.finish_requested => JvmSessionPhase::Ending,
            Some(JvmSessionPhase::Ready) => JvmSessionPhase::Ready,
            _ => JvmSessionPhase::Starting,
        };
        sessions.entry(session.host.as_str()).or_default().push(sync::NodeSession {
            id: id.clone(),
            session_type: session.session_type.clone(),
            key: session.demand_key.clone(),
            players,
            phase: phase.into(),
        });
    }
    let mut nodes = BTreeMap::new();
    for status in statuses {
        let phase = status.phase;
        let draining = phase == NodePhase::Draining;
        let node = sync::Node {
            drain_deadline_ms: draining.then(|| drain_deadline(state, &status.host)).flatten().unwrap_or_default(),
            remaining_claims: remaining.get(status.host.as_str()).copied().filter(|_| retiring(phase)).unwrap_or(0),
            sessions: sessions.remove(status.host.as_str()).filter(|_| phase != NodePhase::Stopped).unwrap_or_default(),
            deployment: status.deployment,
            app: status.app,
            machine_profile: status.profile,
            phase: phase.into(),
            health: status.health,
            observed_at_ms: status.observed_at_ms,
            consecutive_failures: status.consecutive_failures,
        };
        nodes.insert(status.host, node);
    }
    nodes
}
