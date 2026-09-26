//! Each proxy's open claims and pending moves, streamed as the `gateway/<id>` topic sees them.

use chunk_proto::v1::{ClaimIdentity, ClaimPhase, ClaimUpdate, WatchedClaim};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tonic::Status;

use crate::{
    Control,
    gateway::{Delta, View},
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
        let (mut view, mut delta) = match View::open(self, &proxy, None) {
            Ok((view, delta)) => (view, Ok(Some(delta))),
            Err(error) => return drop(sender.send(Err(super::status(error))).await),
        };
        loop {
            // The proxy tracks positions only through claim changes.
            if let Some(update) = delta.map(|delta| delta.filter(|delta| !delta.is_empty())).transpose() {
                let update = update.map(|delta| claim_update(&proxy, delta)).map_err(super::status);
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
            delta = view.next(self);
        }
    }
}

fn claim_update(proxy: &str, delta: Delta) -> ClaimUpdate {
    let claims = delta.upserts.into_iter().map(|(operation, claim)| WatchedClaim {
        claim: Some(ClaimIdentity {
            operation_id: operation,
            proxy_id: proxy.to_owned(),
            membership_generation: claim.membership.wire(),
            delivery_generation: claim.generation.wire(),
        }),
        phase: ClaimPhase::from(claim.phase).into(),
        pending_move: claim.pending_move,
    });
    ClaimUpdate {
        position: delta.position.wire(),
        snapshot: delta.snapshot,
        claims: claims.collect(),
        released: delta.removed,
    }
}
