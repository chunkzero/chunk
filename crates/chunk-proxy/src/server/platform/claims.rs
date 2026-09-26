//! This gateway's claims, as its `gateway/<id>` topic streams them.

use std::{collections::BTreeMap, io, time::Duration};

use chunk_proto::{
    sync::v1::{
        Cursor, GatewayClaim, Position, SubscribeRequest, Update, core_client::CoreClient, entry::State, error::Code,
    },
    v1::ClaimIdentity,
};
use prost::Message;
use tokio::sync::watch;
use tokio_util::sync::{CancellationToken, DropGuard};
use tonic::transport::Channel;

use crate::GatewayCredential;

const RECONNECT_DELAY: Duration = Duration::from_millis(250);
/// Revision bits of control's `uint64` generations, below the epoch.
const REVISION_BITS: u32 = 40;

/// The topic's state at the last update applied. Stale while `live` is false, until the next stream's first update.
#[derive(Default)]
pub(in crate::server) struct View {
    live: bool,
    /// The stream the last update came from.
    stream: String,
    position: Option<Position>,
    claims: BTreeMap<String, GatewayClaim>,
}

impl View {
    /// The open claim `identity` names.
    pub fn claim(&self, identity: &ClaimIdentity) -> Option<&GatewayClaim> {
        self.claims
            .get(&identity.operation_id)
            .filter(|claim| claim.generation.as_ref().map(generation) == Some(identity.delivery_generation))
    }

    /// Whether the claim `identity` names was released: the view has passed its creation without holding it.
    pub fn released(&self, identity: &ClaimIdentity) -> bool {
        self.reached(identity.delivery_generation) && self.claim(identity).is_none()
    }

    /// Whether the view is at or past `position`.
    pub fn passed(&self, position: Option<&Position>) -> bool {
        self.reached(position.map_or(0, generation))
    }

    /// The stream claim calls name, once the view is live.
    pub fn stream(&self) -> Option<&str> {
        Some(self.stream.as_str()).filter(|stream| self.live && !stream.is_empty())
    }

    fn reached(&self, wire: u64) -> bool {
        self.position.as_ref().map_or(0, generation) >= wire
    }

    fn apply(&mut self, update: Update) -> io::Result<()> {
        if !update.stream.is_empty() {
            self.stream = update.stream;
        }
        if update.snapshot {
            self.claims.clear();
        }
        for operation in &update.removed {
            self.claims.remove(operation);
        }
        for entry in update.upserts {
            let Some(State::Value(value)) = entry.state else {
                return Err(io::Error::other("gateway claim entry without a value"));
            };
            self.claims.insert(entry.key, GatewayClaim::decode(value.as_slice()).map_err(io::Error::other)?);
        }
        self.position = update.position;
        Ok(())
    }
}

/// `position` as the `uint64` generation control's legacy messages carry: the epoch above 40 revision bits.
pub(in crate::server) fn generation(position: &Position) -> u64 {
    position.epoch << REVISION_BITS | position.revision
}

/// Follows `gateway`'s topic until the guard drops. A broken stream resumes after the last position applied, and a
/// superseded one starts over from a snapshot.
pub(super) fn follow(client: CoreClient<Channel>, gateway: GatewayCredential) -> (watch::Receiver<View>, DropGuard) {
    let (view, receiver) = watch::channel(View::default());
    let stop = CancellationToken::new();
    let guard = stop.clone().drop_guard();
    tokio::spawn(async move {
        let following = async {
            let mut after = None;
            loop {
                after = stream(client.clone(), &gateway, after, &view).await;
                view.send_modify(|view| view.live = false);
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        };
        tokio::select! { () = stop.cancelled() => {}, () = following => {} }
    });
    (receiver, guard)
}

/// Follows one stream from `after`, returning where the next one resumes.
async fn stream(
    mut client: CoreClient<Channel>,
    gateway: &GatewayCredential,
    after: Option<Cursor>,
    view: &watch::Sender<View>,
) -> Option<Cursor> {
    let mut cursor = after.clone();
    let subscription =
        SubscribeRequest { topic: format!("gateway/{}", gateway.id), after, ..SubscribeRequest::default() };
    let updates = match super::authorized(subscription, &gateway.credential) {
        Ok(request) => client.subscribe(request).await.map_err(io::Error::other),
        Err(error) => Err(error),
    };
    let mut updates = match updates {
        Ok(updates) => updates.into_inner(),
        Err(error) => {
            tracing::debug!(%error, "gateway topic unavailable");
            return cursor;
        }
    };
    let mut pending = Vec::new();
    loop {
        let update = match updates.message().await {
            Ok(Some(update)) => update,
            Ok(None) => return cursor,
            Err(error) => {
                tracing::debug!(%error, "gateway topic interrupted");
                return cursor;
            }
        };
        if let Some(error) = update.error {
            tracing::debug!(message = %error.message, "gateway topic ended");
            return (error.code() != Code::Stopped).then_some(cursor).flatten();
        }
        let continued = update.continued;
        pending.push(update);
        if continued {
            continue;
        }
        let mut applied = Ok(());
        view.send_modify(|view| {
            applied = pending.drain(..).try_for_each(|update| view.apply(update));
            view.live = applied.is_ok();
            cursor = Some(Cursor { stream: view.stream.clone(), position: view.position });
        });
        if let Err(error) = applied {
            tracing::warn!(%error, "invalid gateway topic update");
            return None;
        }
    }
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
