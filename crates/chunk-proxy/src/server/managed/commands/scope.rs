use std::io;

use chunk_proto::sync::v1::ClaimPhase;
use tokio_util::sync::CancellationToken;

use crate::server::{
    claim::{Claim, ClaimIdentity},
    platform::{Platform, RPC_TIMEOUT},
    transport::invalid_data,
};

/// The claim a connection's commands are bound to, from binding until the next configuration.
#[derive(Clone)]
pub(in crate::server::managed) struct Origin {
    pub claim: Claim,
    pub identity: ClaimIdentity,
    /// Names this binding.
    pub id: String,
    /// The player's UUID.
    pub player: String,
    /// The domain of the session's app.
    pub domain: String,
    pub cancellation: CancellationToken,
}
impl Origin {
    pub fn new(claim: &Claim, identity: &ClaimIdentity, domain: String) -> Self {
        Self {
            claim: claim.clone(),
            identity: identity.clone(),
            id: uuid::Uuid::new_v4().to_string(),
            player: claim.player.uuid.clone(),
            domain,
            cancellation: CancellationToken::new(),
        }
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
        self.id == other.id
    }
}
