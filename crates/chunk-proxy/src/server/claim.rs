//! The claims a connection makes on sessions, as this process tracks them.

use chunk_proto::sync::v1::{PlayerIdentity, SessionDemand};

/// A login's or queued move's claim on a session.
#[derive(Clone, Debug, Default)]
pub(super) struct Claim {
    pub operation_id: String,
    /// Names the player's connection to this process; a move keeps its login's.
    pub connection_id: String,
    pub player: PlayerIdentity,
    /// The session the claim places the player in, which a login's routing chooses.
    pub demand: SessionDemand,
    /// The arrived claim a move leaves; `None` for a login.
    pub source: Option<ClaimIdentity>,
    /// The deployment whose hooks admitted the claim. Empty places it with the current one.
    pub deployment: String,
    /// Stops a login from asking to return to the session its player left.
    pub decline_reconnect: bool,
    /// The session of an earlier deployment the claim's deployment admitted the login to as a reconnect.
    pub reconnect: String,
}

/// A claim as this gateway's claim view names it: its operation at the generation that delivered it.
#[derive(Clone, Debug)]
pub(super) struct ClaimIdentity {
    pub operation_id: String,
    pub delivery_generation: u64,
}
