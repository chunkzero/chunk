//! This gateway's claims, as its `gateway/<id>` topic streams them.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    time::Duration,
};

use chunk_proto::{
    sync::v1::{
        ClaimPhase, Cursor, GatewayArguments, GatewayClaim, Position, SubscribeRequest, Update,
        core_client::CoreClient, entry::State, error::Code,
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
    /// This process's instance, which owns the topic and starts the connection IDs it names.
    instance: String,
    live: bool,
    /// Another process under this gateway's ID took the topic over, so the follower follows no more.
    replaced: bool,
    /// The stream the last update came from, cleared whenever it ends, before the follower subscribes again.
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

    /// The operations of the open claims no withdrawal has reached that another process under this gateway's ID
    /// created: their connection IDs aren't this process's. A move's claim keeps its source's connection ID, so
    /// control's moves of this process's players are its own.
    pub fn inherited(&self) -> BTreeSet<String> {
        let prefix = connection_prefix(&self.instance);
        let inherited = self
            .claims
            .iter()
            .filter(|(_, claim)| claim.phase() != ClaimPhase::Withdrawing && !claim.connection_id.starts_with(&prefix));
        inherited.map(|(operation, _)| operation.clone()).collect()
    }

    /// Whether the view is at or past `position`.
    pub fn passed(&self, position: Option<&Position>) -> bool {
        self.reached(position.map_or(0, generation))
    }

    /// The stream claim calls name, once the view is live.
    pub fn stream(&self) -> Option<&str> {
        Some(self.stream.as_str()).filter(|stream| self.live && !stream.is_empty())
    }

    /// Whether `stream` may have been superseded: the view no longer names it. Only this gateway's one follower
    /// supersedes its streams, and it leaves each one before subscribing again.
    pub fn superseded(&self, stream: &str) -> bool {
        self.stream != stream
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

/// A new connection ID of process `instance`.
pub(super) fn connection_id(instance: &str) -> String {
    connection_prefix(instance) + &uuid::Uuid::new_v4().to_string()
}

fn connection_prefix(instance: &str) -> String {
    format!("{instance}/")
}

/// `position` as the `uint64` generation control's legacy messages carry: the epoch above 40 revision bits.
pub(in crate::server) fn generation(position: &Position) -> u64 {
    position.epoch << REVISION_BITS | position.revision
}

/// Follows `gateway`'s topic as process `instance` until the guard drops, or until another process under the gateway's
/// ID takes the topic over. A broken stream resumes after the last position applied, and a stream core stopped starts
/// over from a snapshot.
pub(super) fn follow(
    client: CoreClient<Channel>,
    gateway: GatewayCredential,
    instance: String,
) -> (watch::Receiver<View>, DropGuard) {
    let (view, receiver) = watch::channel(View { instance, ..View::default() });
    let stop = CancellationToken::new();
    let guard = stop.clone().drop_guard();
    tokio::spawn(async move {
        let following = async {
            let mut after = None;
            loop {
                after = stream(client.clone(), &gateway, after, &view).await;
                view.send_modify(|view| {
                    view.live = false;
                    view.stream.clear();
                });
                if view.borrow().replaced {
                    return;
                }
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
    let arguments = GatewayArguments { instance: view.borrow().instance.clone() }.encode_to_vec();
    let subscription =
        SubscribeRequest { topic: format!("gateway/{}", gateway.id), arguments, after, ..SubscribeRequest::default() };
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
            match error.code() {
                Code::Superseded => view.send_modify(|view| view.replaced = true),
                Code::Stopped => return None,
                _ => {}
            }
            return cursor;
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

/// Waits for a live view in which `ready` returns a value. Fails once another process under this gateway's ID took the
/// topic over.
pub(super) async fn wait<T>(
    mut view: watch::Receiver<View>,
    mut ready: impl FnMut(&View) -> Option<T>,
) -> io::Result<T> {
    let mut result = None;
    view.wait_for(|view| {
        result = if view.replaced {
            Some(Err(io::Error::other("another process under this gateway's ID replaced this one")))
        } else if view.live {
            ready(view).map(Ok)
        } else {
            None
        };
        result.is_some()
    })
    .await
    .map_err(io::Error::other)?;
    result.unwrap_or_else(|| Err(io::Error::other("claim view closed")))
}
