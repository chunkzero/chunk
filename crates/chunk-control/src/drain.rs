use chunk_proto::control::v1::ClaimRequest;
use prost::Message;
use std::time::Duration;
use tokio::task::JoinSet;

use crate::{
    Control, Error, MoveRequest, Result,
    state::{Capacity, Claim, Drain, Phase, State},
};

/// Records a drain once per operation, retiring the resolved host and its sessions until the deadline.
pub(crate) fn retire_host(
    state: &mut State,
    operation: String,
    request: Vec<u8>,
    timeout_seconds: u32,
    automatic: bool,
    host: impl FnOnce(&State) -> Result<String>,
) -> Result<()> {
    if let Some(drain) = state.drains.get(&operation) {
        return if drain.request == request { Ok(()) } else { Err(Error::Invalid(crate::DRAIN_CHANGED)) };
    }
    if state.drains.len() >= 256 {
        return Err(Error::Capacity);
    }
    let host = host(state)?;
    state.hosts.get_mut(&host).ok_or(Error::Invalid("unknown host"))?.retired = true;
    for session in state.sessions.values_mut().filter(|s| s.host == host) {
        session.retired = true;
    }
    let deadline_ms = crate::now_ms() + u64::from(timeout_seconds) * 1000;
    state.drains.insert(operation, Drain { request, host, deadline_ms, automatic });
    Ok(())
}

impl Control {
    /// Waits for `tasks`, advancing drains every second so evacuation deadlines still fire.
    pub(crate) async fn join_progressing_drains(&self, mut tasks: JoinSet<()>) -> Result<()> {
        let mut drains = tokio::time::interval(Duration::from_secs(1));
        while !tasks.is_empty() {
            tokio::select! {
                _ = tasks.join_next() => {}
                _ = drains.tick() => self.progress_drains()?,
            }
        }
        Ok(())
    }

    /// Moves each draining host's arrived players away, and releases the host's capacity once it is empty or its
    /// deadline passed.
    pub(crate) fn progress_drains(&self) -> Result<()> {
        let state = self.state()?;
        let mut releasing = Vec::new();
        for drain in
            state.drains.values().filter(|d| state.hosts.get(&d.host).is_some_and(|h| h.capacity < Capacity::Releasing))
        {
            let claims: Vec<_> = open_claims(&state, &drain.host).collect();
            if claims.is_empty() || crate::now_ms() >= drain.deadline_ms {
                releasing.push(drain.host.clone());
            } else {
                for claim in claims.iter().filter(|c| c.phase == Phase::Arrived) {
                    let source = ClaimRequest::decode(claim.request.as_slice())?;
                    let result = self.move_player(MoveRequest {
                        operation_id: uuid::Uuid::new_v4().to_string(),
                        player_id: claim.player.clone(),
                        demand: source.demand.unwrap_or_default(),
                        source: None,
                    });
                    if let Err(error) = result {
                        tracing::debug!(%error, "drain awaits pending player move");
                    }
                }
            }
        }
        if releasing.is_empty() {
            return Ok(());
        }
        self.update(|state| {
            for id in &releasing {
                if let Some(host) = state.hosts.get_mut(id).filter(|host| host.capacity < Capacity::Releasing) {
                    host.capacity = Capacity::Releasing;
                }
            }
            Ok(())
        })?;
        self.wake_capacity();
        Ok(())
    }
}

/// The host serving `player`'s current claim, which must not be retired already.
pub(crate) fn player_host(state: &State, player: &str) -> Result<String> {
    let owner = state
        .players
        .get(player)
        .and_then(|p| p.current.as_ref())
        .ok_or(Error::Invalid("player has no current runtime"))?;
    let host = &state.sessions[&state.claims[owner].session].host;
    if state.hosts[host].retired {
        return Err(Error::Invalid("runtime already retired"));
    }
    Ok(host.clone())
}

fn open_claims<'a>(state: &'a State, host: &'a str) -> impl Iterator<Item = &'a Claim> {
    state.claims.values().filter(move |c| c.phase != Phase::Released && state.sessions[&c.session].host == host)
}
