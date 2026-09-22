use std::io;

use chunk_proto::v1::{Assignment, ClaimIdentity, ClaimPhase, ClaimRequest, CommandScope};
use tokio_util::sync::CancellationToken;

use crate::server::{platform::Platform, transport::invalid_data};

#[derive(Clone)]
pub(in crate::server::managed) struct Origin {
    pub claim: ClaimRequest,
    pub identity: ClaimIdentity,
    pub scope: CommandScope,
    pub cancellation: CancellationToken,
}
impl Origin {
    pub fn new(claim: &ClaimRequest, assignment: &Assignment, domain: String) -> io::Result<Self> {
        let player = claim.identity.as_ref().ok_or_else(|| invalid_data("missing command player"))?;
        let demand = claim.demand.as_ref().ok_or_else(|| invalid_data("missing command destination"))?;
        let identity = assignment.claim.clone().ok_or_else(|| invalid_data("missing command claim"))?;
        let delivery = assignment.delivery.as_ref().ok_or_else(|| invalid_data("missing command delivery"))?;
        let session = delivery.session.as_ref().ok_or_else(|| invalid_data("missing command session"))?;
        let (app, _) = demand.session_type.split_once('/').ok_or_else(|| invalid_data("missing command app"))?;
        Ok(Self {
            claim: claim.clone(),
            scope: CommandScope {
                proxy_id: claim.proxy_id.clone(),
                player_uuid: player.uuid.clone(),
                username: player.username.clone(),
                session_id: session.id.clone(),
                app: app.into(),
                session_type: demand.session_type.clone(),
                domain,
                scope_id: uuid::Uuid::new_v4().to_string(),
                connection_id: claim.connection_id.clone(),
                claim_operation_id: identity.operation_id.clone(),
                membership_generation: identity.membership_generation,
                delivery_generation: identity.delivery_generation,
            },
            identity,
            cancellation: CancellationToken::new(),
        })
    }
    pub async fn inspect(&self, platform: &Platform) -> io::Result<()> {
        let assignment = platform
            .control
            .clone()
            .inspect(platform.control_request(self.claim.clone())?)
            .await
            .map_err(io::Error::other)?
            .into_inner();
        let expected = &platform.target.backend;
        if assignment.phase != i32::from(ClaimPhase::Arrived)
            || assignment.claim.as_ref() != Some(&self.identity)
            || assignment.delivery.as_ref().is_none_or(|delivery| {
                delivery.connection_id != self.claim.connection_id
                    || delivery.proxy_id != self.claim.proxy_id
                    || delivery.operation_id != self.identity.operation_id
                    || delivery.membership_generation != self.identity.membership_generation
                    || delivery.owner_generation != self.identity.delivery_generation
                    || delivery.player.as_ref().is_none_or(|player| player.id != self.scope.player_uuid)
                    || delivery.session.as_ref().is_none_or(|session| session.id != self.scope.session_id)
                    || delivery.deployment.as_ref().is_none_or(|deployment| {
                        deployment.environment != expected.environment || deployment.deployment != expected.deployment
                    })
            })
        {
            return Err(invalid_data("command scope no longer arrived"));
        }
        Ok(())
    }
    pub fn matches(&self, other: &Self) -> bool {
        self.scope.scope_id == other.scope.scope_id
    }
}
