//! Each proxy's open claims and pending moves, streamed from control's change feed.

use std::collections::{BTreeMap, BTreeSet};

use chunk_proto::v1::{ClaimPhase, ClaimRequest, ClaimUpdate, WatchedClaim};
use prost::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::{
    Change, Control, Generation, Result, Table,
    state::{Phase, State},
};

impl Control {
    /// Sends `proxy`'s open claims until `sender` or `closed` closes: a snapshot, then changes. Each update waits for
    /// the reader, so a slow one receives only each claim's latest state, and one that falls behind retained history
    /// receives a new snapshot.
    pub(crate) async fn watch(
        &self,
        proxy: String,
        sender: mpsc::Sender<std::result::Result<ClaimUpdate, Status>>,
        closed: CancellationToken,
    ) {
        let mut positions = self.subscribe();
        let mut view = View { proxy, sent: BTreeMap::new(), position: Generation::default() };
        let mut update = self.state().and_then(|state| view.snapshot(&state)).map(Some);
        loop {
            if let Some(update) = update.transpose() {
                let update = update.map_err(super::status);
                let failed = update.is_err();
                let sent = tokio::select! { () = closed.cancelled() => return, sent = sender.send(update) => sent };
                if sent.is_err() || failed {
                    return;
                }
            }
            tokio::select! {
                () = closed.cancelled() => return,
                () = sender.closed() => return,
                changed = positions.changed() => if changed.is_err() { return },
            }
            update = match self.changes_after(view.position) {
                Some(changes) => self.state().and_then(|state| view.apply(&state, &changes)),
                None => self.state().and_then(|state| view.snapshot(&state)).map(Some),
            };
        }
    }
}

/// The claims one watcher was last sent.
struct View {
    proxy: String,
    sent: BTreeMap<String, WatchedClaim>,
    /// Every change up to this position is reflected in `sent`.
    position: Generation,
}

impl View {
    fn snapshot(&mut self, state: &State) -> Result<ClaimUpdate> {
        let moves = pending_moves(state)?;
        self.sent = state
            .claims
            .keys()
            .filter_map(|operation| Some((operation.clone(), watched(state, &moves, operation, &self.proxy)?)))
            .collect();
        self.position = state.position();
        Ok(ClaimUpdate {
            position: self.position.wire(),
            snapshot: true,
            claims: self.sent.values().cloned().collect(),
            released: Vec::new(),
        })
    }

    /// This proxy's claims that `changes` affected, read from `state`, which includes them. `None` when no change
    /// concerns this proxy.
    fn apply(&mut self, state: &State, changes: &[Change]) -> Result<Option<ClaimUpdate>> {
        let mut affected = BTreeSet::new();
        for change in changes {
            self.position = self.position.max(change.position);
            affected.insert(change.id.clone());
            // A move and its destination claim decide whether their source has a pending move.
            let request = match change.table {
                Table::Claims => state.claims.get(&change.id).map(|claim| &claim.request),
                Table::Moves => state.moves.get(&change.id).map(|intent| &intent.request),
            };
            if let Some(request) = request
                && let Some(source) = ClaimRequest::decode(request.as_slice())?.source
            {
                affected.insert(source.operation_id);
            }
        }
        affected.retain(|operation| {
            self.sent.contains_key(operation)
                || state.claims.get(operation).is_some_and(|claim| claim.proxy == self.proxy)
        });
        if affected.is_empty() {
            return Ok(None);
        }
        let moves = pending_moves(state)?;
        let mut update = ClaimUpdate { position: self.position.wire(), ..ClaimUpdate::default() };
        for operation in affected {
            match watched(state, &moves, &operation, &self.proxy) {
                Some(claim) if self.sent.get(&operation) != Some(&claim) => {
                    update.claims.push(claim.clone());
                    self.sent.insert(operation, claim);
                }
                Some(_) => {}
                None => {
                    if self.sent.remove(&operation).is_some() {
                        update.released.push(operation);
                    }
                }
            }
        }
        Ok(Some(update))
    }
}

fn watched(
    state: &State,
    moves: &BTreeMap<String, ClaimRequest>,
    operation: &str,
    proxy: &str,
) -> Option<WatchedClaim> {
    let claim = state.claims.get(operation).filter(|claim| claim.proxy == proxy && claim.phase != Phase::Released)?;
    Some(WatchedClaim {
        claim: Some(claim.identity(operation)),
        phase: ClaimPhase::from(claim.phase).into(),
        pending_move: moves.get(operation).cloned(),
    })
}

/// The destination of each arrived claim's pending move, by source operation.
pub(crate) fn pending_moves(state: &State) -> Result<BTreeMap<String, ClaimRequest>> {
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
