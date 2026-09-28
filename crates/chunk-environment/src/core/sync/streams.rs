//! Subscription streams: their IDs, a sender that coalesces what a slow client has yet to take, message splitting, and
//! nudges after a credential's own writes. Entry values are shared, so streams of the same results hold each value once
//! until they encode it. Each message is charged its encoded bytes against the send budget's [stream share] before the
//! client takes it, and the charge moves through the stream's [`Ledger`] into the response frame that carries it.
//! While the share has no room, changes keep coalescing, newer ones merging into the refused message, and the charge
//! is retried every [`RETRY`]; a stream still refused after [`DEADLINE`] ends with OVERLOADED. A stream's final
//! update overdraws the budget rather than wait, so each stream holds at most one such message beyond it.
//!
//! [stream share]: SendBudget::streams
//!
//! A stream sends position-only updates at most every [`ADVANCE_INTERVAL`], so a slow client's advances coalesce
//! sooner. Rust clients multiplexing many independently-consumed streams on one connection should still raise h2's
//! `data_frame_budget` or lower their stream window, since a stalled stream's small frames can exceed the budget.

mod parts;

use super::{
    auth::{hex, hmac},
    errors,
    transport::{Ledger, PREFIX_BYTES},
};
use chunk_backend::{SendBudget, SendCharge};
use chunk_proto::sync::v1::{Entry, Error, Position, SubscribeRequest, Update, error::Code};
use chunk_service::same_secret;
use chunk_store::Revision;
use parts::{Batch, Part};
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, ready},
    time::Duration,
};
use tokio::{
    sync::{Notify, mpsc, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;
use tonic::Status;

/// The hex nonce that starts every stream ID.
const NONCE_BYTES: usize = 32;

/// The least time between a stream's sends when the later one only advances its position.
const ADVANCE_INTERVAL: Duration = Duration::from_millis(50);
/// How often a message the send budget had no room for is charged again.
const RETRY: Duration = Duration::from_millis(50);
/// How long a message is charged again before its stream ends with OVERLOADED.
const DEADLINE: Duration = Duration::from_secs(5);

/// Keys stream IDs for this process only, so a stream from before a restart never resumes.
pub(super) struct StreamKey([u8; 32]);

impl StreamKey {
    pub fn new() -> Self {
        let mut key = [0; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(key)
    }

    /// A new stream's ID: a fresh nonce, then an HMAC binding it to the subscription's scope and credential.
    pub fn id(&self, request: &SubscribeRequest, credential: &str) -> String {
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mac = self.mac(&nonce, request, credential);
        nonce + &mac
    }

    /// Whether this process issued `id` for a stream of `request`'s scope and `credential`.
    pub fn verify(&self, id: &str, request: &SubscribeRequest, credential: &str) -> bool {
        id.split_at_checked(NONCE_BYTES)
            .is_some_and(|(nonce, mac)| same_secret(mac, &self.mac(nonce, request, credential)))
    }

    /// An HMAC-SHA256 over `nonce` and the subscription's topic, arguments, deployment, caller and credential.
    fn mac(&self, nonce: &str, request: &SubscribeRequest, credential: &str) -> String {
        let caller = request.caller.as_ref();
        let fields = [
            nonce.as_bytes(),
            request.topic.as_bytes(),
            &request.arguments,
            request.deployment.as_bytes(),
            caller.map_or(&[][..], |caller| caller.session.as_bytes()),
            caller.map_or(&[][..], |caller| caller.player.as_bytes()),
            credential.as_bytes(),
        ];
        let mut message = Vec::new();
        for field in fields {
            message.extend_from_slice(&(field.len() as u64).to_be_bytes());
            message.extend_from_slice(field);
        }
        hex(&hmac(&self.0, &message))
    }
}

/// The newest stream of each fenced topic, and the process instances that owned it, by topic.
#[derive(Default)]
pub(super) struct Fences(Mutex<HashMap<String, Fence>>);

#[derive(Default)]
struct Fence {
    stream: String,
    credential: String,
    superseded: CancellationToken,
    /// The process instance that opened the current stream.
    instance: String,
    /// Cancelled once another instance takes the topic over.
    retired: CancellationToken,
    /// Instances another took the topic over from, which never open it again.
    retirees: HashSet<String>,
}

/// A stream's current place on its fenced topic.
pub(super) struct Fenced {
    /// Cancelled once a newer stream supersedes this one.
    pub superseded: CancellationToken,
    /// Cancelled, before `superseded` is, once another instance takes the topic over.
    pub retired: CancellationToken,
}

impl Fences {
    /// Makes `stream`, which process `instance` opened, `topic`'s current stream, superseding the earlier one. An
    /// instance new to the topic takes it over from the current one, which is retired. Fails with SUPERSEDED for an
    /// instance already retired.
    pub fn fence(&self, topic: &str, instance: &str, stream: &str, credential: &str) -> Result<Fenced, Error> {
        let mut fences = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let fence = fences.entry(topic.to_owned()).or_default();
        if fence.retirees.contains(instance) {
            return Err(errors::error(Code::Superseded, "a later process under this gateway's ID took its topic over"));
        }
        if fence.instance != instance {
            fence.retired.cancel();
            let retired = std::mem::replace(&mut fence.instance, instance.to_owned());
            if !retired.is_empty() {
                fence.retirees.insert(retired);
            }
            fence.retired = CancellationToken::new();
        }
        fence.superseded.cancel();
        fence.superseded = CancellationToken::new();
        stream.clone_into(&mut fence.stream);
        credential.clone_into(&mut fence.credential);
        Ok(Fenced { superseded: fence.superseded.clone(), retired: fence.retired.clone() })
    }

    /// Checks that `stream` is the current stream of a fenced topic `credential` opened.
    pub fn check(&self, stream: &str, credential: &str) -> Result<(), Error> {
        self.follow(stream, credential).map(drop)
    }

    /// Checks that `stream` is the current stream of a fenced topic `credential` opened, returning the token
    /// cancelled once a newer stream supersedes it.
    pub fn follow(&self, stream: &str, credential: &str) -> Result<CancellationToken, Error> {
        let fences = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = fences.values().find(|fence| fence.stream == stream && fence.credential == credential);
        let current = current.map(|fence| fence.superseded.clone());
        current.ok_or_else(|| errors::error(Code::Stopped, "the stream was superseded or is unknown"))
    }
}

/// The latest revision each credential's mutations committed, which the streams it opened catch up to promptly.
#[derive(Default)]
pub(super) struct Nudges(Mutex<HashMap<String, watch::Sender<Revision>>>);

impl Nudges {
    /// Follows the commits of `credential`'s later mutations.
    pub fn follow(&self, credential: &str) -> watch::Receiver<Revision> {
        let mut nudges = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        nudges.retain(|_, sender| !sender.is_closed());
        nudges.entry(credential.to_owned()).or_insert_with(|| watch::Sender::new(Revision(0))).subscribe()
    }

    pub fn nudge(&self, credential: &str, revision: Revision) {
        let nudges = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sender) = nudges.get(credential) {
            sender.send_if_modified(|latest| {
                let later = revision > *latest;
                *latest = (*latest).max(revision);
                later
            });
        }
    }
}

/// A stream's consuming side, which moves the charge of each update it yields into its ledger.
pub(crate) struct Stream {
    parts: mpsc::Receiver<Part>,
    ledger: Ledger,
}

impl Stream {
    /// The ledger the response body takes the charges of encoded updates from.
    pub(super) fn ledger(&self) -> Ledger {
        self.ledger.clone()
    }
}

impl tokio_stream::Stream for Stream {
    type Item = Result<Update, Status>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Some(Part { update, charge }) = ready!(self.parts.poll_recv(context)) else {
            return Poll::Ready(None);
        };
        if let Some(charge) = charge {
            self.ledger.push(charge);
        }
        Poll::Ready(Some(Ok(update)))
    }
}

/// A stream's producing side, which never waits for the client: each update merges into the one the client has yet
/// to take, and a writer task hands that over as the client reads.
pub(super) struct Sender {
    slot: Arc<Slot>,
    closed: CancellationToken,
}

/// A stream whose messages are charged against `budget`.
pub(super) fn channel(budget: SendBudget) -> (Sender, Stream) {
    let (sender, parts) = mpsc::channel(1);
    let slot = Arc::<Slot>::default();
    let closed = CancellationToken::new();
    drop(tokio::spawn(write(slot.clone(), sender, closed.clone(), budget)));
    (Sender { slot, closed }, Stream { parts, ledger: Ledger::default() })
}

impl Sender {
    /// Resolves once the client went away or took the stream's final update.
    pub async fn closed(&self) {
        self.closed.cancelled().await;
    }

    /// Merges `update` into what the client has yet to take.
    pub fn send(&self, update: Update) {
        self.slot.update(|pending| {
            if pending.error.is_none() {
                pending.changes.get_or_insert_default().merge(update);
            }
        });
    }

    /// Replaces what the client has yet to take with snapshot `update`, which holds `charge`, if any, until it's
    /// replaced or the charge grows to cover its messages.
    pub fn send_snapshot(&self, update: Update, charge: Option<SendCharge>) {
        self.slot.update(|pending| {
            if pending.error.is_none() {
                let changes = pending.changes.get_or_insert_default();
                changes.merge(Update { snapshot: true, ..update });
                changes.charge = charge;
            }
        });
    }

    /// Ends the stream with `error`, which replaces anything the client has yet to take and is sent as soon as it
    /// has room.
    pub fn fail(&self, error: Error) {
        self.slot.update(|pending| {
            pending.changes = None;
            pending.error.get_or_insert(error);
        });
    }

    /// Ends the stream once the client took what it has yet to take.
    pub fn finish(&self) {
        self.slot.update(|pending| pending.ended = true);
    }

    /// Ends the stream with `error` if the client has room for it, or else at once, releasing everything the client
    /// has yet to take.
    pub fn end(self, error: Error) {
        self.slot.update(|pending| {
            pending.changes = None;
            pending.error.get_or_insert(error);
            pending.abandoned = true;
        });
    }
}

impl Drop for Sender {
    fn drop(&mut self) {
        self.slot.update(|pending| pending.ended = true);
    }
}

#[derive(Default)]
struct Slot {
    pending: Mutex<Pending>,
    wake: Notify,
}

#[derive(Default)]
struct Pending {
    changes: Option<Changes>,
    /// The stream's final update, sent in place of any changes.
    error: Option<Error>,
    /// The sender dropped; the writer ends once it sent the rest.
    ended: bool,
    /// The writer ends without waiting for the client to have room.
    abandoned: bool,
}

impl Slot {
    fn update(&self, change: impl FnOnce(&mut Pending)) {
        change(&mut self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        self.wake.notify_one();
    }
}

/// Updates the client has yet to take, merged into one: the latest value of each key at the latest position.
#[derive(Default)]
struct Changes {
    position: Option<Position>,
    snapshot: bool,
    stream: String,
    upserts: BTreeMap<String, Entry>,
    removed: BTreeSet<String>,
    /// Charged for the snapshot the changes hold.
    charge: Option<SendCharge>,
}

impl Changes {
    fn merge(&mut self, update: Update) {
        if update.snapshot {
            *self = Self { snapshot: true, stream: std::mem::take(&mut self.stream), ..Self::default() };
        }
        if !update.stream.is_empty() {
            self.stream = update.stream;
        }
        self.position = update.position.or(self.position);
        for entry in update.upserts {
            self.removed.remove(&entry.key);
            self.upserts.insert(entry.key.clone(), entry);
        }
        for key in update.removed {
            self.upserts.remove(&key);
            if !self.snapshot {
                self.removed.insert(key);
            }
        }
    }

    fn only_advances(&self) -> bool {
        !self.snapshot && self.upserts.is_empty() && self.removed.is_empty()
    }

    fn into_update(self) -> Update {
        Update {
            position: self.position,
            snapshot: self.snapshot,
            upserts: self.upserts.into_values().collect(),
            removed: self.removed.into_iter().collect(),
            stream: self.stream,
            ..Update::default()
        }
    }
}

/// Hands the slot's changes to `client` as it takes them, split to fit in messages and charged against `budget`, and
/// ends after the slot's error, or once the stream is abandoned while the client has no room. Changes that only
/// advance the position wait out [`ADVANCE_INTERVAL`] since the previous send, and changes the budget has no room for
/// take in newer ones until it has.
async fn write(slot: Arc<Slot>, client: mpsc::Sender<Part>, closed: CancellationToken, budget: SendBudget) {
    let _closed = closed.drop_guard();
    let mut batch = Batch::default();
    let mut advance_at = Instant::now();
    loop {
        let permit = tokio::select! {
            permit = client.reserve() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
            () = slot.wake.notified() => {
                if slot.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).abandoned {
                    return;
                }
                continue;
            }
        };
        let (part, last) = loop {
            let wake_at;
            {
                let mut pending = slot.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(error) = pending.error.take() {
                    break (last(&budget, error), true);
                }
                let paced = !pending.ended && Instant::now() < advance_at;
                if batch.is_empty()
                    && let Some(changes) = pending.changes.take_if(|changes| !(paced && changes.only_advances()))
                {
                    batch = Batch::new(changes);
                } else if batch.refused().is_some()
                    && let Some(changes) = pending.changes.take()
                {
                    batch.merge(changes);
                }
                wake_at = match batch.next(&budget) {
                    Ok(Some(part)) => break (part, false),
                    Ok(None) if pending.ended => return,
                    Ok(None) => pending.changes.is_some().then_some(advance_at),
                    Err(failure) if batch.refused().is_some_and(|refused| refused.elapsed() >= DEADLINE) => {
                        pending.changes = None;
                        break (last(&budget, errors::backend(&failure)), true);
                    }
                    Err(_) => Some(Instant::now() + RETRY),
                };
            }
            tokio::select! {
                () = slot.wake.notified() => {}
                () = tokio::time::sleep_until(wake_at.unwrap_or_else(Instant::now)), if wake_at.is_some() => {}
                () = client.closed() => return,
            }
        };
        permit.send(part);
        advance_at = Instant::now() + ADVANCE_INTERVAL;
        if last {
            return;
        }
    }
}

/// The stream's final update, with `error`, charged even beyond the budget so it never waits for room.
fn last(budget: &SendBudget, error: Error) -> Part {
    let update = Update { error: Some(error), ..Update::default() };
    let charge = budget.overdraw(update.encoded_len() + PREFIX_BYTES);
    Part { update, charge: Some(charge) }
}

#[cfg(test)]
mod tests;
