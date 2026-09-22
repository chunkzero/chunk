use chunk_proto::v1::{ClaimRequest, DrainRequest, DrainStatus, MovePlayerRequest};
use prost::Message;

use crate::{
    Control, Error, Result,
    state::{Drain, Phase, State},
};

/// Records a drain once per operation, retiring the resolved host and its sessions until the deadline.
pub(crate) fn retire_host(
    state: &mut State,
    operation: String,
    request: Vec<u8>,
    timeout_seconds: u32,
    host: impl FnOnce(&State) -> Result<String>,
) -> Result<()> {
    if let Some(drain) = state.drains.get(&operation) {
        return if drain.request == request { Ok(()) } else { Err(Error::Invalid("drain changed")) };
    }
    if state.drains.len() >= 256 {
        return Err(Error::Capacity);
    }
    let host = host(state)?;
    state.hosts.get_mut(&host).ok_or(Error::Invalid("unknown host"))?.retired = true;
    for session in state.sessions.values_mut().filter(|s| s.host == host) {
        session.retired = true;
    }
    state
        .drains
        .insert(operation, Drain { request, host, deadline_ms: crate::now_ms() + u64::from(timeout_seconds) * 1000 });
    Ok(())
}

impl Control {
    /// Retires a runtime's capacity before queuing moves, retaining a durable shutdown deadline.
    /// # Errors
    /// Rejects changed operations, unknown players and unbounded drain deadlines.
    pub fn drain(&self, request: DrainRequest) -> Result<DrainStatus> {
        if request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || !(10..=120).contains(&request.timeout_seconds)
        {
            return Err(Error::Invalid("invalid drain request"));
        }
        self.update(|state| {
            retire_host(
                state,
                request.operation_id.clone(),
                request.encode_to_vec(),
                request.timeout_seconds,
                |state| {
                    let owner = state
                        .players
                        .get(&request.player_id)
                        .and_then(|p| p.current.as_ref())
                        .ok_or(Error::Invalid("player has no current runtime"))?;
                    let host = &state.sessions[&state.claims[owner].session].host;
                    if state.hosts[host].retired {
                        return Err(Error::Invalid("runtime already retired"));
                    }
                    Ok(host.clone())
                },
            )
        })?;
        let state = self.state()?;
        let drain = &state.drains[&request.operation_id];
        Ok(DrainStatus {
            operation_id: request.operation_id,
            host_id: drain.host.clone(),
            deadline_ms: drain.deadline_ms,
            remaining_claims: state
                .claims
                .values()
                .filter(|c| c.phase != Phase::Released && state.sessions[&c.session].host == drain.host)
                .count()
                .try_into()
                .map_err(|_| Error::Capacity)?,
            stopped: self.host.stopped(&drain.host),
        })
    }

    pub(crate) async fn progress_drains(&self) -> Result<()> {
        let state = self.state()?;
        for drain in state.drains.values().filter(|d| !self.host.stopped(&d.host)) {
            let claims: Vec<_> = state
                .claims
                .values()
                .filter(|c| c.phase != Phase::Released && state.sessions[&c.session].host == drain.host)
                .collect();
            if claims.is_empty() || crate::now_ms() >= drain.deadline_ms {
                if let Err(error) = self.host.terminate(&drain.host).await {
                    tracing::warn!(%error, host = %drain.host, "drain termination unresolved; retaining ownership");
                }
            } else {
                for claim in claims.iter().filter(|c| c.phase == Phase::Arrived) {
                    let source = ClaimRequest::decode(claim.request.as_slice())?;
                    let result = self.move_player(MovePlayerRequest {
                        expected_source: None,
                        expected_connection_id: String::new(),
                        operation_id: uuid::Uuid::new_v4().to_string(),
                        player_id: claim.player.clone(),
                        demand: source.demand,
                    });
                    if let Err(error) = result {
                        tracing::debug!(%error, "drain awaits pending player move");
                    }
                }
            }
        }
        Ok(())
    }
}
