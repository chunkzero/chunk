//! Atomic roster reservation: a group moves to one destination session together, or not at all.
//!
//! Reservation queues every member's move and reserves every slot in one commit; any failing member rejects the
//! whole roster. Session capacity is the only hard limit. Admission waits until every member has asked to activate,
//! then activates all of them in one commit. Before admission, a member's claim ending (cancellation, expiry or
//! disconnect) or [`Control::cancel_roster`] fails the roster: remaining members' moves fail and reconciliation
//! withdraws their claims. Members that already left their source then have no destination, as with any failed move.
//! After admission, members are ordinary single players.

use std::collections::BTreeSet;

use chunk_proto::v1::{ClaimIdentity, ClaimRequest, MovePlayerRequest, SessionDemand};
use prost::Message;

use crate::{
    Control, Error, Result,
    placement::{insert_claim, owner, select_room},
    state::{MoveFailure, Phase, Roster, State},
};

pub struct RosterMember {
    /// The member's destination claim operation.
    pub operation_id: String,
    pub player_id: String,
    /// The exact arrived claim the caller captured, and its public connection.
    pub expected_source: ClaimIdentity,
    pub expected_connection_id: String,
}

pub struct RosterMove {
    pub operation_id: String,
    /// The caller's membership version. A retry must repeat the version and members exactly.
    pub version: u64,
    pub demand: SessionDemand,
    pub members: Vec<RosterMember>,
}

impl Control {
    /// Reserves one destination session slot and queues a move for every member, or changes nothing.
    /// # Errors
    /// Rejects stale or moving members and changed retries; reports `Capacity` when no session can hold every
    /// member and `Busy` when too much work is in flight.
    pub fn move_roster(&self, request: &RosterMove) -> Result<Vec<ClaimRequest>> {
        let players: BTreeSet<_> = request.members.iter().map(|member| &member.player_id).collect();
        let operations: BTreeSet<_> = request.members.iter().map(|member| &member.operation_id).collect();
        if request.operation_id.is_empty()
            || request.operation_id.len() > 128
            || request.members.is_empty()
            || players.len() != request.members.len()
            || operations.len() != request.members.len()
            || i64::try_from(request.version).is_err()
        {
            return Err(Error::Invalid("invalid roster"));
        }
        let _operation = self.operation(&request.operation_id)?;
        if !self.state()?.rosters.contains_key(&request.operation_id) && !self.recovery.open()? {
            return Err(Error::Busy);
        }
        let unavailable = self.unavailable()?;
        self.update(|state| {
            if self.draining.load(std::sync::atomic::Ordering::Acquire) {
                return Err(Error::Invalid("control draining"));
            }
            if let Some(roster) = state.rosters.get(&request.operation_id) {
                return retried(state, roster, request);
            }
            let mut destinations = Vec::with_capacity(request.members.len());
            for member in &request.members {
                let command = MovePlayerRequest {
                    operation_id: member.operation_id.clone(),
                    player_id: member.player_id.clone(),
                    demand: Some(request.demand.clone()),
                    expected_source: Some(member.expected_source.clone()),
                    expected_connection_id: member.expected_connection_id.clone(),
                };
                destinations.push(crate::moves::queue(state, &self.config, command)?);
            }
            let session = select_room(state, &self.config, &request.demand, &unavailable, destinations.len())?;
            for destination in &destinations {
                let owner = owner(state, destination)?;
                insert_claim(state, destination, owner, session.clone(), Some(request.operation_id.clone()))?;
            }
            let members = destinations.iter().map(|destination| destination.operation_id.clone()).collect();
            let roster = Roster { version: request.version, members, ready: Vec::new(), admitted: false };
            state.rosters.insert(request.operation_id.clone(), roster);
            Ok(destinations)
        })
    }

    /// Fails a roster that has not been admitted, so members stay where they are.
    /// # Errors
    /// Rejects unknown and already admitted rosters.
    pub fn cancel_roster(&self, operation_id: &str) -> Result<()> {
        self.update(|state| {
            let roster = state.rosters.get(operation_id).ok_or(Error::Invalid("unknown roster"))?;
            if roster.admitted {
                return Err(Error::Invalid("roster already admitted"));
            }
            fail(state, operation_id, "roster canceled");
            Ok(())
        })
    }
}

fn retried(state: &State, roster: &Roster, request: &RosterMove) -> Result<Vec<ClaimRequest>> {
    let same = roster.version == request.version
        && roster.members.iter().eq(request.members.iter().map(|member| &member.operation_id));
    if !same {
        return Err(Error::Invalid("roster operation changed"));
    }
    request
        .members
        .iter()
        .map(|member| {
            let intent = state.moves.get(&member.operation_id).ok_or(Error::Invalid("missing roster move"))?;
            let destination = ClaimRequest::decode(intent.request.as_slice())?;
            if destination.identity.as_ref().map(|identity| &identity.uuid) != Some(&member.player_id)
                || destination.demand.as_ref() != Some(&request.demand)
                || destination.source.as_ref() != Some(&member.expected_source)
            {
                return Err(Error::Invalid("roster operation changed"));
            }
            Ok(destination)
        })
        .collect()
}

/// Records `member`'s activation request. Returns the members to activate now: none while others are pending, all
/// of them when the last becomes ready, or just `member` once the roster was admitted.
pub(crate) fn ready(state: &mut State, roster: &str, member: &str) -> Result<Vec<String>> {
    let entry = state.rosters.get(roster).ok_or(Error::Invalid("unknown roster"))?;
    if entry.admitted {
        return Ok(vec![member.into()]);
    }
    let complete = entry.members.iter().all(|member| {
        state.claims.get(member).is_some_and(|claim| !matches!(claim.phase, Phase::Withdrawing | Phase::Released))
            && state.moves.get(member).is_some_and(|intent| !intent.canceled && intent.failure.is_none())
    });
    if !complete {
        return Err(Error::Invalid("roster incomplete"));
    }
    let entry = state.rosters.get_mut(roster).ok_or(Error::Invalid("unknown roster"))?;
    if !entry.ready.iter().any(|ready| ready == member) {
        entry.ready.push(member.into());
    }
    if entry.ready.len() < entry.members.len() {
        return Ok(Vec::new());
    }
    entry.admitted = true;
    Ok(entry.members.clone())
}

/// Fails every member's move of a roster that has not been admitted.
pub(crate) fn fail(state: &mut State, roster: &str, reason: &str) {
    let Some(entry) = state.rosters.get(roster).filter(|entry| !entry.admitted) else {
        return;
    };
    let at_ms = crate::now_ms();
    for member in &entry.members {
        if let Some(intent) = state.moves.get_mut(member) {
            intent.failure.get_or_insert_with(|| MoveFailure { reason: reason.into(), at_ms });
        }
    }
}
