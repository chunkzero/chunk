use std::io;

use chunk_proto::{
    sync::v1::ClaimPhase,
    v1::{ClaimIdentity, ClaimRequest, CommandScope},
};
use tokio_util::sync::CancellationToken;

use crate::server::{
    platform::{Platform, RPC_TIMEOUT},
    transport::invalid_data,
};

#[derive(Clone)]
pub(in crate::server::managed) struct Origin {
    pub claim: ClaimRequest,
    pub identity: ClaimIdentity,
    pub scope: CommandScope,
    pub cancellation: CancellationToken,
}
impl Origin {
    pub fn new(claim: &ClaimRequest, identity: &ClaimIdentity, session: &str, domain: String) -> io::Result<Self> {
        let player = claim.identity.as_ref().ok_or_else(|| invalid_data("missing command player"))?;
        let demand = claim.demand.as_ref().ok_or_else(|| invalid_data("missing command destination"))?;
        let (app, _) = demand.session_type.split_once('/').ok_or_else(|| invalid_data("missing command app"))?;
        Ok(Self {
            claim: claim.clone(),
            scope: CommandScope {
                proxy_id: claim.proxy_id.clone(),
                player_uuid: player.uuid.clone(),
                username: player.username.clone(),
                session_id: session.into(),
                app: app.into(),
                session_type: demand.session_type.clone(),
                domain,
                scope_id: uuid::Uuid::new_v4().to_string(),
                connection_id: claim.connection_id.clone(),
                claim_operation_id: identity.operation_id.clone(),
                membership_generation: identity.membership_generation,
                delivery_generation: identity.delivery_generation,
            },
            identity: identity.clone(),
            cancellation: CancellationToken::new(),
        })
    }
    /// Confirms from the claim view that this scope's claim is still arrived under the same identity.
    pub async fn check(&self, platform: &Platform) -> io::Result<()> {
        let phase = platform.claims(|view| Some(view.claim(&self.identity).map(|claim| claim.phase)));
        match tokio::time::timeout(RPC_TIMEOUT, phase).await {
            Ok(Ok(Some(phase))) if phase == i32::from(ClaimPhase::Arrived) => Ok(()),
            Ok(Ok(_)) => Err(invalid_data("command scope no longer arrived")),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "claim view unavailable")),
        }
    }
    pub fn matches(&self, other: &Self) -> bool {
        self.scope.scope_id == other.scope.scope_id
    }
}
