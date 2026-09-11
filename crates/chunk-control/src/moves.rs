use chunk_proto::v1::{ClaimIdentity, ClaimRequest, MovePlayerRequest, PendingMove};
use prost::Message;

use crate::{
    Control, Error, Result,
    state::{MoveIntent, Phase, State},
};

impl Control {
    /// Queues one move for the proxy that owns the player's public connection.
    /// # Errors
    /// Rejects changed operations, concurrent moves and players without an arrived delivery.
    pub fn move_player(&self, request: MovePlayerRequest) -> Result<ClaimRequest> {
        if request.operation_id.is_empty() || request.operation_id.len() > 128 || request.demand.is_none() {
            return Err(Error::Invalid("invalid move request"));
        }
        self.update(|state| {
            // A retry must recover the original source even after arrival in the destination.
            if let Some(intent) = state.moves.get(&request.operation_id) {
                let previous = ClaimRequest::decode(intent.request.as_slice())?;
                if previous.identity.as_ref().map(|i| &i.uuid) != Some(&request.player_id)
                    || previous.demand != request.demand
                {
                    return Err(Error::Invalid("move operation changed"));
                }
                return Ok(previous);
            }
            if state.moves.len() >= 1024 || state.claims.contains_key(&request.operation_id) {
                return Err(Error::Capacity);
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
            destination.demand = request.demand;
            destination.source = Some(claim.identity(source));
            state.moves.insert(
                destination.operation_id.clone(),
                MoveIntent { request: destination.encode_to_vec(), canceled: false },
            );
            Ok(destination)
        })
    }

    /// Returns the pending move only to the exact current claim.
    /// # Errors
    /// Rejects unknown or changed claims.
    pub fn poll_move(&self, request: &ClaimRequest) -> Result<PendingMove> {
        let state = self.state()?;
        let source = state.claims.get(&request.operation_id).ok_or(Error::Invalid("unknown source"))?;
        source.matches(request)?;
        let mut result = PendingMove::default();
        if source.phase == Phase::Arrived {
            for intent in state.moves.values().filter(|intent| !intent.canceled) {
                let destination = ClaimRequest::decode(intent.request.as_slice())?;
                if destination.source.as_ref() == Some(&source.identity(&request.operation_id))
                    && state.claims.get(&destination.operation_id).is_none_or(|c| c.phase != Phase::Released)
                {
                    result.claim = Some(destination);
                    break;
                }
            }
        }
        Ok(result)
    }

    pub(crate) fn cancel_intent(&self, request: &ClaimRequest) -> Result<bool> {
        self.update(|state| {
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

pub(crate) fn authorize_destination(state: &mut State, identity: &ClaimIdentity) -> Result<()> {
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
