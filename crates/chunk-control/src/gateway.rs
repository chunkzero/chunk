//! The `gateway/<id>` sync topic: the open claims one gateway holds, keyed by operation ID, with the moves pending
//! from them. Values are `chunk.sync.v1.GatewayClaim` messages.

use std::collections::{BTreeMap, BTreeSet};

use chunk_proto::{sync::v1 as sync, v1::ClaimRequest};
use prost::Message;

use crate::{
    Control, Generation, Result,
    state::{Phase, State, feed::Change},
};

/// One stream of a gateway's topic.
pub struct Topic {
    view: View,
}

impl Topic {
    /// Opens `gateway`'s topic and returns its first update: the changes after `after` when that position is within
    /// retained history, or else a snapshot. Either way it reflects the current position.
    /// # Errors
    /// Reports unreadable control state.
    pub fn open(control: &Control, gateway: &str, after: Option<Generation>) -> Result<(Self, sync::Update)> {
        let (view, delta) = View::open(control, gateway, after)?;
        Ok((Self { view }, delta.update()))
    }

    /// The update since the previous one, or `None` when control has not committed since. A commit that touched none
    /// of the gateway's claims only advances the position, and one outside retained history sends a new snapshot.
    /// # Errors
    /// Reports unreadable control state.
    pub fn next(&mut self, control: &Control) -> Result<Option<sync::Update>> {
        Ok(self.view.next(control)?.map(Delta::update))
    }
}

/// A gateway's claims as one stream last sent them.
pub(crate) struct View {
    gateway: String,
    /// The value last sent for each key, so an unchanged claim is not sent again.
    sent: BTreeMap<String, Watched>,
    /// The position of the last update.
    position: Generation,
}

/// An open claim as its gateway sees it.
#[derive(Clone, PartialEq)]
pub(crate) struct Watched {
    pub generation: Generation,
    pub phase: Phase,
    /// The destination of the move pending from this arrived claim.
    pub pending_move: Option<ClaimRequest>,
}

/// What changed since a stream's previous update, keyed by operation ID.
pub(crate) struct Delta {
    pub position: Generation,
    pub snapshot: bool,
    pub upserts: Vec<(String, Watched)>,
    pub removed: Vec<String>,
}

impl View {
    pub fn open(control: &Control, gateway: &str, after: Option<Generation>) -> Result<(Self, Delta)> {
        let mut view = Self { gateway: gateway.to_owned(), sent: BTreeMap::new(), position: Generation::default() };
        let resumed = after.and_then(|after| Some((after, control.authority.feed().after(after)?)));
        let delta = if let Some((after, (changes, state))) = resumed {
            view.position = after;
            view.changes(&state, &changes)?
        } else {
            let state = control.state()?;
            view.snapshot(&state)?
        };
        Ok((view, delta))
    }

    pub fn next(&mut self, control: &Control) -> Result<Option<Delta>> {
        match control.authority.feed().after(self.position) {
            Some((_, state)) if state.position() == self.position => Ok(None),
            Some((changes, state)) => self.changes(&state, &changes).map(Some),
            None => {
                let state = control.state()?;
                self.snapshot(&state).map(Some)
            }
        }
    }

    fn snapshot(&mut self, state: &State) -> Result<Delta> {
        let moves = pending_moves(state)?;
        self.sent = state
            .claims
            .keys()
            .filter_map(|operation| Some((operation.clone(), watched(state, &moves, operation, &self.gateway)?)))
            .collect();
        self.position = state.position();
        let upserts = self.sent.iter().map(|(operation, claim)| (operation.clone(), claim.clone())).collect();
        Ok(Delta { position: self.position, snapshot: true, upserts, removed: Vec::new() })
    }

    /// The gateway's claims that `changes` touched, read from `state`, which they lead to. A touched claim no longer
    /// held is removed whether or not this stream sent it, since a resumed stream's client may hold it.
    fn changes(&mut self, state: &State, changes: &[Change]) -> Result<Delta> {
        let touched: BTreeSet<_> =
            changes.iter().filter(|change| change.gateway == self.gateway).map(|change| &change.claim).collect();
        let moves = if touched.is_empty() { BTreeMap::new() } else { pending_moves(state)? };
        self.position = state.position();
        let mut delta = Delta { position: self.position, snapshot: false, upserts: Vec::new(), removed: Vec::new() };
        for operation in touched {
            match watched(state, &moves, operation, &self.gateway) {
                Some(claim) if self.sent.get(operation) != Some(&claim) => {
                    delta.upserts.push((operation.clone(), claim.clone()));
                    self.sent.insert(operation.clone(), claim);
                }
                Some(_) => {}
                None => {
                    self.sent.remove(operation);
                    delta.removed.push(operation.clone());
                }
            }
        }
        Ok(delta)
    }
}

impl Delta {
    fn update(self) -> sync::Update {
        let upserts = self.upserts.into_iter().map(|(operation, claim)| {
            let value = sync::GatewayClaim {
                generation: position(claim.generation),
                phase: phase(claim.phase).into(),
                pending_move: claim.pending_move.map(|destination| sync::GatewayMove {
                    operation_id: destination.operation_id,
                    destination: destination.demand.map(|demand| sync::SessionDemand {
                        key: demand.key,
                        session_type: demand.session_type,
                        machine_profile: demand.machine_profile,
                    }),
                }),
            };
            sync::Entry { key: operation, state: Some(sync::entry::State::Value(value.encode_to_vec())) }
        });
        sync::Update {
            position: position(self.position),
            snapshot: self.snapshot,
            upserts: upserts.collect(),
            removed: self.removed,
            ..sync::Update::default()
        }
    }
}

pub(crate) fn position(generation: Generation) -> Option<sync::Position> {
    (generation.revision != 0).then_some(sync::Position { epoch: generation.epoch, revision: generation.revision })
}

pub(crate) fn phase(phase: Phase) -> sync::ClaimPhase {
    match phase {
        Phase::Reserved => sync::ClaimPhase::Reserved,
        Phase::Activating => sync::ClaimPhase::Activating,
        Phase::Attached => sync::ClaimPhase::Attached,
        Phase::Arrived => sync::ClaimPhase::Arrived,
        Phase::Withdrawing => sync::ClaimPhase::Withdrawing,
        // Released claims leave the topic.
        Phase::Released => sync::ClaimPhase::Unspecified,
    }
}

fn watched(state: &State, moves: &BTreeMap<String, ClaimRequest>, operation: &str, gateway: &str) -> Option<Watched> {
    let claim = state.claims.get(operation).filter(|claim| claim.proxy == gateway && claim.phase != Phase::Released)?;
    Some(Watched { generation: claim.generation, phase: claim.phase, pending_move: moves.get(operation).cloned() })
}

/// The destination of each arrived claim's pending move, by source operation.
fn pending_moves(state: &State) -> Result<BTreeMap<String, ClaimRequest>> {
    let mut pending = BTreeMap::new();
    for intent in state.moves.values().filter(|intent| !intent.canceled && intent.failure.is_none()) {
        let destination = ClaimRequest::decode(intent.request.as_slice())?;
        let Some(source) = &destination.source else {
            continue;
        };
        let current = state
            .claims
            .get(&source.operation_id)
            .is_some_and(|claim| claim.phase == Phase::Arrived && claim.identity(&source.operation_id) == *source);
        let open = state
            .claims
            .get(&destination.operation_id)
            .is_none_or(|claim| !matches!(claim.phase, Phase::Withdrawing | Phase::Released));
        if current && open {
            pending.entry(source.operation_id.clone()).or_insert(destination);
        }
    }
    Ok(pending)
}
