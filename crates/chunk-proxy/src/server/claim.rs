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
    /// The deployment whose routing chose a login's demand. Empty places it with the current one.
    pub deployment: String,
}

/// A claim as this gateway's claim view names it: its operation at the generation that delivered it.
#[derive(Clone, Debug)]
pub(super) struct ClaimIdentity {
    pub operation_id: String,
    pub delivery_generation: u64,
}
