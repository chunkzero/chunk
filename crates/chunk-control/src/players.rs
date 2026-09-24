use chunk_proto::v1::{ClaimPhase, ClaimRequest, MoveFailure, PlayerList, PlayerStatus};
use prost::Message;

use crate::{Control, Result, state::Phase};

impl Control {
    /// Every player that currently owns a delivery, with the session and node serving it.
    /// # Errors
    /// Reports unavailable or undecodable durable state.
    pub fn players(&self) -> Result<PlayerList> {
        let state = self.state()?;
        let moves = state
            .moves
            .values()
            .map(|intent| Ok((intent, ClaimRequest::decode(intent.request.as_slice())?)))
            .collect::<Result<Vec<_>>>()?;
        let mut players = Vec::new();
        for owner in state.players.values() {
            let Some(claim) = owner.current.as_ref().and_then(|operation| state.claims.get(operation)) else {
                continue;
            };
            let request = ClaimRequest::decode(claim.request.as_slice())?;
            let source = claim.identity(&request.operation_id);
            let latest = moves
                .iter()
                .filter(|(_, destination)| destination.source.as_ref() == Some(&source))
                .max_by_key(|(intent, _)| intent.sequence);
            let last_move_failure = latest.and_then(|(intent, destination)| {
                intent.failure.as_ref().map(|failure| MoveFailure {
                    destination: destination.demand.clone(),
                    reason: failure.reason.clone(),
                    failed_at_ms: failure.at_ms,
                })
            });
            let queued = latest.is_some_and(|(intent, destination)| {
                !intent.canceled
                    && intent.failure.is_none()
                    && state
                        .claims
                        .get(&destination.operation_id)
                        .is_none_or(|c| !matches!(c.phase, Phase::Withdrawing | Phase::Released))
            });
            let host_id = state.sessions.get(&claim.session).map(|session| session.host.clone()).unwrap_or_default();
            players.push(PlayerStatus {
                identity: request.identity,
                demand: request.demand,
                app_id: state.hosts.get(&host_id).map(|host| host.app.clone()).unwrap_or_default(),
                host_id,
                phase: ClaimPhase::from(claim.phase).into(),
                moving: owner.pending.is_some() || queued,
                since_ms: claim.created_at_ms,
                last_move_failure,
            });
        }
        Ok(PlayerList { players })
    }
}
