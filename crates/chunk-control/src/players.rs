use chunk_proto::v1::{ClaimPhase, ClaimRequest, PlayerList, PlayerStatus};
use prost::Message;

use crate::{Control, Result};

impl Control {
    /// Every player that currently owns a delivery, with the session and node serving it.
    /// # Errors
    /// Reports unavailable or undecodable durable state.
    pub fn players(&self) -> Result<PlayerList> {
        let state = self.state()?;
        let mut players = Vec::new();
        for owner in state.players.values() {
            let Some(claim) = owner.current.as_ref().and_then(|operation| state.claims.get(operation)) else {
                continue;
            };
            let request = ClaimRequest::decode(claim.request.as_slice())?;
            let host_id = state.sessions.get(&claim.session).map(|session| session.host.clone()).unwrap_or_default();
            players.push(PlayerStatus {
                identity: request.identity,
                demand: request.demand,
                app_id: state.hosts.get(&host_id).map(|host| host.app.clone()).unwrap_or_default(),
                host_id,
                phase: ClaimPhase::from(claim.phase).into(),
                moving: owner.pending.is_some(),
                since_ms: claim.created_at_ms,
            });
        }
        Ok(PlayerList { players })
    }
}
