use chunk_proto::v1::{ClaimIdentity, ClaimRequest, SessionDemand};
use prost::Message;

use crate::{
    Control, Error, Generation, Result,
    state::{MoveFailure, MoveIntent, Phase, State},
};

/// A move of `player_id` to a session meeting `demand`, whose destination claim takes `operation_id`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MoveRequest {
    pub operation_id: String,
    pub player_id: String,
    pub demand: SessionDemand,
    /// Fences the move to the claim and public connection the caller captured, as proxy effects do.
    pub source: Option<MoveSource>,
}

/// The player's arrived claim and public connection a move was requested from.
#[derive(Clone, Debug, PartialEq)]
pub struct MoveSource {
    pub claim: ClaimIdentity,
    pub connection_id: String,
}

impl Control {
    /// Queues one move for the proxy that owns the player's public connection.
    /// # Errors
    /// Rejects changed operations, concurrent moves and players without an arrived delivery.
    pub fn move_player(&self, request: MoveRequest) -> Result<ClaimRequest> {
        validate(&request)?;
        self.update(|state| queue(state, request))
    }

    /// Records `reason` as why the move to `claim` ended before activation, leaving fenced withdrawal to
    /// reconciliation.
    /// # Errors
    /// Rejects changed or activated moves and failures to persist the report.
    pub async fn abandon_move(&self, claim: ClaimRequest, reason: String) -> Result<ClaimIdentity> {
        if reason.is_empty() || reason.len() > 4096 {
            return Err(Error::Invalid("invalid move failure reason"));
        }
        self.cancel_with_failure(claim, Some(reason)).await
    }

    pub(crate) fn cancel_intent(&self, request: &ClaimRequest, failure: Option<String>) -> Result<bool> {
        self.update(|state| {
            if let Some(reason) = failure {
                let intent = state.moves.get_mut(&request.operation_id).ok_or(Error::Invalid("unknown move"))?;
                if intent.request != request.encode_to_vec() {
                    return Err(Error::Invalid("move changed"));
                }
                if state.claims.get(&request.operation_id).is_some_and(|claim| claim.activated) {
                    return Err(Error::Invalid("move already activated"));
                }
                intent.failure.get_or_insert(MoveFailure { reason, at_ms: crate::now_ms() });
            }
            if state.claims.contains_key(&request.operation_id) {
                return Ok(false);
            }
            let intent = state.moves.get_mut(&request.operation_id).ok_or(Error::Invalid("unknown claim"))?;
            if intent.request != request.encode_to_vec() {
                return Err(Error::Invalid("move changed"));
            }
            intent.canceled = true;
            Ok(true)
        })
    }
}

pub(crate) fn validate(request: &MoveRequest) -> Result<()> {
    if request.operation_id.is_empty()
        || request.operation_id.len() > 128
        || request
            .source
            .as_ref()
            .is_some_and(|source| source.connection_id.is_empty() || source.connection_id.len() > 128)
    {
        return Err(Error::Invalid("invalid move request"));
    }
    Ok(())
}

/// Queues `request`'s move within its source's release in the current update, returning the destination claim request.
pub(crate) fn queue(state: &mut State, request: MoveRequest) -> Result<ClaimRequest> {
    if let Some(expected) = &request.source {
        let claim = state.arrived_claim(&expected.claim).ok_or(Error::Invalid("stale captured move source"))?;
        if claim.player != request.player_id
            || ClaimRequest::decode(claim.request.as_slice())?.connection_id != expected.connection_id
        {
            return Err(Error::Invalid("stale captured move source"));
        }
    }
    // Trusted unbound retries can recover the source after arrival in the destination.
    if let Some(intent) = state.moves.get(&request.operation_id) {
        let previous = ClaimRequest::decode(intent.request.as_slice())?;
        if previous.identity.as_ref().map(|i| &i.uuid) != Some(&request.player_id)
            || previous.demand.as_ref() != Some(&request.demand)
            || request.source.as_ref().is_some_and(|expected| {
                previous.source.as_ref() != Some(&expected.claim) || previous.connection_id != expected.connection_id
            })
        {
            return Err(Error::Invalid(crate::MOVE_CHANGED));
        }
        return Ok(previous);
    }
    if state.claims.contains_key(&request.operation_id) {
        return Err(Error::Invalid(crate::MOVE_NAMES_CLAIM));
    }
    let owner = state.players.get(&request.player_id).ok_or(Error::Invalid("unknown player"))?;
    let source = owner.current.as_ref().ok_or(Error::Invalid("player has no current delivery"))?;
    let claim = &state.claims[source];
    if claim.phase != Phase::Arrived || owner.pending.is_some() {
        return Err(Error::Invalid("player already transitioning"));
    }
    for intent in state.moves.values().filter(|intent| !intent.canceled) {
        let queued = ClaimRequest::decode(intent.request.as_slice())?;
        if queued.source.as_ref().map(|s| &s.operation_id) == Some(source)
            && state.claims.get(&queued.operation_id).is_none_or(|c| c.phase != Phase::Released)
        {
            return Err(Error::Invalid("move already queued"));
        }
    }
    let mut destination = ClaimRequest::decode(claim.request.as_slice())?;
    destination.operation_id = request.operation_id;
    destination.demand = Some(request.demand);
    destination.source = Some(claim.identity(source));
    let (_, release) = state.placing(&destination)?;
    crate::placement::validate_demand(
        &release,
        destination.demand.as_ref().ok_or(Error::Invalid("missing destination"))?,
    )?;
    let sequence = Generation::PENDING.wire();
    state.moves.insert(
        destination.operation_id.clone(),
        MoveIntent { request: destination.encode_to_vec(), canceled: false, sequence, failure: None },
    );
    Ok(destination)
}

pub(crate) fn authorize_destination(state: &mut State, identity: &ClaimIdentity) -> Result<()> {
    if state.moves.get(&identity.operation_id).is_some_and(|intent| intent.canceled || intent.failure.is_some()) {
        return Err(Error::Invalid("move abandoned"));
    }
    let claim = state.claims.get(&identity.operation_id).ok_or(Error::Invalid("unknown claim"))?;
    if claim.identity(&identity.operation_id) != *identity {
        return Err(Error::Invalid("stale destination"));
    }
    let request = ClaimRequest::decode(claim.request.as_slice())?;
    if let Some(source) = request.source {
        let previous = state.claims.get(&source.operation_id).ok_or(Error::Invalid("unknown source"))?;
        if previous.identity(&source.operation_id) != source || previous.phase != Phase::Released {
            return Err(Error::Unresolved("move source is not fenced"));
        }
        let owner = state.players.get_mut(&claim.player).ok_or(Error::Invalid("missing owner"))?;
        if owner.pending.as_ref() == Some(&identity.operation_id) && owner.current.is_none() {
            owner.current = owner.pending.take();
        }
    }
    Ok(())
}
