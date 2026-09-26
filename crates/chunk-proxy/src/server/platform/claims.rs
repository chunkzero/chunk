//! This proxy's claims, as control streams them.

use std::{collections::BTreeMap, io, time::Duration};

use chunk_proto::v1::{
    ClaimIdentity, ClaimUpdate, WatchRequest, WatchedClaim, local_control_client::LocalControlClient,
};
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::transport::Channel;

const RECONNECT_DELAY: Duration = Duration::from_millis(250);

/// The latest claim states control sent. Stale while `live` is false, until the next snapshot replaces it.
#[derive(Default)]
pub(in crate::server) struct View {
    live: bool,
    /// The control commit the view reflects, in wire generation form.
    position: u64,
    claims: BTreeMap<String, WatchedClaim>,
}

impl View {
    /// The open claim `identity` names.
    pub fn claim(&self, identity: &ClaimIdentity) -> Option<&WatchedClaim> {
        self.claims.get(&identity.operation_id).filter(|claim| claim.claim.as_ref() == Some(identity))
    }

    /// Whether the claim `identity` names was released: the view has passed its creation without holding it.
    pub fn released(&self, identity: &ClaimIdentity) -> bool {
        self.position >= identity.delivery_generation && self.claim(identity).is_none()
    }

    #[cfg(test)]
    pub fn position(&self) -> u64 {
        self.position
    }

    fn apply(&mut self, update: ClaimUpdate) {
        if update.snapshot {
            self.claims.clear();
            self.live = true;
        }
        for operation in &update.released {
            self.claims.remove(operation);
        }
        for claim in update.claims {
            if let Some(identity) = &claim.claim {
                self.claims.insert(identity.operation_id.clone(), claim);
            }
        }
        self.position = update.position;
    }
}

/// Follows control's claim stream for `proxy_id` until the guard drops, reconnecting after each failure.
pub(super) fn follow(
    control: LocalControlClient<Channel>,
    proxy_id: String,
    token: String,
) -> (watch::Receiver<View>, DropGuard) {
    let (view, receiver) = watch::channel(View::default());
    let stop = CancellationToken::new();
    let guard = stop.clone().drop_guard();
    tokio::spawn(async move {
        let reconnecting = async {
            loop {
                let request = super::authorized(WatchRequest { proxy_id: proxy_id.clone() }, &token);
                if let Err(error) = stream(control.clone(), request, &view).await {
                    tracing::debug!(%error, "claim stream interrupted");
                }
                view.send_modify(|view| view.live = false);
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        };
        tokio::select! { () = stop.cancelled() => {}, () = reconnecting => {} }
    });
    (receiver, guard)
}

async fn stream(
    mut control: LocalControlClient<Channel>,
    request: io::Result<tonic::Request<WatchRequest>>,
    view: &watch::Sender<View>,
) -> io::Result<()> {
    let mut updates = control.watch(request?).await.map_err(io::Error::other)?.into_inner();
    while let Some(update) = updates.message().await.map_err(io::Error::other)? {
        view.send_modify(|view| view.apply(update));
    }
    Ok(())
}

/// Waits for a live view in which `ready` returns a value.
pub(super) async fn wait<T>(
    mut view: watch::Receiver<View>,
    mut ready: impl FnMut(&View) -> Option<T>,
) -> io::Result<T> {
    let mut result = None;
    view.wait_for(|view| {
        result = if view.live { ready(view) } else { None };
        result.is_some()
    })
    .await
    .map_err(io::Error::other)?;
    result.ok_or_else(|| io::Error::other("claim view closed"))
}
