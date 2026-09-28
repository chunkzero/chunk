//! Subscription streams: their IDs, a sender that coalesces what a slow client has yet to take, message splitting, and
//! nudges after a credential's own writes. An update sent with a charge holds it until the client took the next, or
//! the stream is gone.
//!
//! A stream sends position-only updates at most every [`ADVANCE_INTERVAL`], so a slow client's advances coalesce
//! sooner. Rust clients multiplexing many independently-consumed streams on one connection should still raise h2's
//! `data_frame_budget` or lower their stream window, since a stalled stream's small frames can exceed the budget.

use super::{
    MESSAGE_BYTES,
    auth::{hex, hmac},
    errors,
};
use chunk_backend::RequestCharge;
use chunk_proto::sync::v1::{Entry, Error, Position, SubscribeRequest, Update, entry::State, error::Code};
use chunk_service::same_secret;
use chunk_store::Revision;
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
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

/// Room left in each message for its position, flags and stream ID.
const HEADER_BYTES: usize = 2048;

/// The hex nonce that starts every stream ID.
const NONCE_BYTES: usize = 32;

/// The least time between a stream's sends when the later one only advances its position.
const ADVANCE_INTERVAL: Duration = Duration::from_millis(50);

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

/// The newest stream of each fenced topic, by topic.
#[derive(Default)]
pub(super) struct Fences(Mutex<HashMap<String, Fence>>);

struct Fence {
    stream: String,
    credential: String,
    superseded: CancellationToken,
}

impl Fences {
    /// Makes `stream` `topic`'s current stream, superseding the earlier one, and returns the token cancelled once a
    /// newer stream supersedes this one.
    pub fn fence(&self, topic: &str, stream: &str, credential: &str) -> CancellationToken {
        let superseded = CancellationToken::new();
        let fence =
            Fence { stream: stream.to_owned(), credential: credential.to_owned(), superseded: superseded.clone() };
        let mut fences = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(previous) = fences.insert(topic.to_owned(), fence) {
            previous.superseded.cancel();
        }
        superseded
    }

    /// Whether a stream other than `stream` has since become `topic`'s current one.
    pub fn superseded(&self, topic: &str, stream: &str) -> bool {
        let fences = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        fences.get(topic).is_some_and(|fence| fence.stream != stream)
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

/// A stream's consuming side, which holds the charge of the update the client took last.
pub(crate) struct Stream {
    parts: mpsc::Receiver<Part>,
    taken: Option<RequestCharge>,
}

/// An update handed to the client, and the charge for what it holds.
struct Part {
    update: Update,
    charge: Option<RequestCharge>,
}

impl tokio_stream::Stream for Stream {
    type Item = Result<Update, Status>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let part = ready!(self.parts.poll_recv(context));
        let (update, charge) = part.map(|part| (part.update, part.charge)).unzip();
        self.taken = charge.flatten();
        Poll::Ready(update.map(Ok))
    }
}

/// A stream's producing side, which never waits for the client: each update merges into the one the client has yet
/// to take, and a writer task hands that over as the client reads.
pub(super) struct Sender {
    slot: Arc<Slot>,
    closed: CancellationToken,
}

pub(super) fn channel() -> (Sender, Stream) {
    let (sender, parts) = mpsc::channel(1);
    let slot = Arc::<Slot>::default();
    let closed = CancellationToken::new();
    drop(tokio::spawn(write(slot.clone(), sender, closed.clone())));
    (Sender { slot, closed }, Stream { parts, taken: None })
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

    /// Replaces what the client has yet to take with snapshot `update`, which holds `charge` until the client took it
    /// or it's dropped.
    pub fn send_snapshot(&self, update: Update, charge: RequestCharge) {
        self.slot.update(|pending| {
            if pending.error.is_none() {
                let changes = pending.changes.get_or_insert_default();
                changes.merge(Update { snapshot: true, ..update });
                changes.charge = Some(charge);
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
    charge: Option<RequestCharge>,
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

/// Hands the slot's changes to `client` as it takes them, split to fit in messages, and ends after the slot's error,
/// or once the stream is abandoned while the client has no room. Changes that only advance the position wait out
/// [`ADVANCE_INTERVAL`] since the previous send. The changes' charge goes with their last part.
async fn write(slot: Arc<Slot>, client: mpsc::Sender<Part>, closed: CancellationToken) {
    let _closed = closed.drop_guard();
    let (mut parts, mut charge) = (VecDeque::new(), None);
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
            let held;
            {
                let mut pending = slot.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(error) = pending.error.take() {
                    break (Update { error: Some(error), ..Update::default() }, true);
                }
                let paced = !pending.ended && Instant::now() < advance_at;
                if parts.is_empty()
                    && let Some(mut changes) = pending.changes.take_if(|changes| !(paced && changes.only_advances()))
                {
                    charge = changes.charge.take();
                    parts = split(changes.into_update()).into();
                }
                if let Some(part) = parts.pop_front() {
                    break (part, false);
                }
                if pending.ended {
                    return;
                }
                held = pending.changes.is_some();
            }
            tokio::select! {
                () = slot.wake.notified() => {}
                () = tokio::time::sleep_until(advance_at), if held => {}
                () = client.closed() => return,
            }
        };
        let charge = if parts.is_empty() { charge.take() } else { None };
        permit.send(Part { update: part, charge });
        advance_at = Instant::now() + ADVANCE_INTERVAL;
        if last {
            return;
        }
    }
}

/// Splits `update` so each part fits in a message. Every part but the last is `continued`, and only the first
/// carries the snapshot flag and stream ID.
fn split(mut update: Update) -> Vec<Update> {
    if update.encoded_len() <= MESSAGE_BYTES {
        return vec![update];
    }
    let upserts = std::mem::take(&mut update.upserts);
    let removed = std::mem::take(&mut update.removed);
    let position = update.position;
    let mut parts = vec![update];
    let mut size = 0;
    for mut entry in upserts {
        if entry.encoded_len() + 16 > MESSAGE_BYTES - HEADER_BYTES {
            let error = errors::invalid("the entry exceeds the 16 MiB message limit");
            entry.state = Some(State::Error(error));
        }
        let bytes = entry.encoded_len() + 16;
        room(&mut parts, &mut size, bytes, position).upserts.push(entry);
    }
    for key in removed {
        let bytes = key.len() + 16;
        room(&mut parts, &mut size, bytes, position).removed.push(key);
    }
    parts
}

/// The last part if it has room for `bytes` more, or else a new part continuing it.
fn room<'a>(parts: &'a mut Vec<Update>, size: &mut usize, bytes: usize, position: Option<Position>) -> &'a mut Update {
    let last = parts.last_mut().expect("a part");
    if *size + bytes > MESSAGE_BYTES - HEADER_BYTES && !(last.upserts.is_empty() && last.removed.is_empty()) {
        last.continued = true;
        parts.push(Update { position, ..Update::default() });
        *size = 0;
    }
    *size += bytes;
    parts.last_mut().expect("a part")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upsert(key: &str, value: &str, revision: u64) -> Update {
        Update {
            position: Some(Position { epoch: 1, revision }),
            upserts: vec![Entry { key: key.into(), state: Some(State::Value(value.as_bytes().to_vec())) }],
            ..Update::default()
        }
    }

    #[test]
    fn merged_changes_keep_every_key_at_its_latest_value() {
        let mut changes = Changes::default();
        changes.merge(upsert("a", "1", 2));
        changes.merge(upsert("b", "1", 3));
        changes.merge(upsert("a", "2", 4));
        changes.merge(Update {
            position: Some(Position { epoch: 1, revision: 5 }),
            removed: vec!["b".into()],
            ..Update::default()
        });

        let update = changes.into_update();
        assert_eq!(update.position, Some(Position { epoch: 1, revision: 5 }));
        let values: Vec<_> = update.upserts.iter().map(|entry| (entry.key.as_str(), entry.state.clone())).collect();
        assert_eq!(values, [("a", Some(State::Value(b"2".to_vec())))]);
        assert_eq!(update.removed, ["b"]);
    }

    #[tokio::test]
    async fn ending_a_stalled_stream_releases_what_its_client_has_yet_to_take() {
        let (sender, mut stream) = channel();
        sender.send(upsert("a", "1", 1));
        while stream.parts.is_empty() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        sender.send(upsert("b", "1", 2));
        sender.end(errors::invalid("replaced"));
        let released = async {
            while !stream.parts.is_closed() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(1), released).await.expect("the writer ended without a reader");
        assert_eq!(stream.parts.recv().await.unwrap().update.upserts[0].key, "a");
        assert!(stream.parts.recv().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn position_only_updates_are_paced_but_data_is_not() {
        let (sender, mut stream) = channel();
        let mut received = 0;
        for revision in 1..=500 {
            sender.send(Update { position: Some(Position { epoch: 1, revision }), ..Update::default() });
            tokio::time::sleep(Duration::from_millis(1)).await;
            while stream.parts.try_recv().is_ok() {
                received += 1;
            }
        }
        assert!((10..=11).contains(&received), "{received} advances in 500 ms");
        tokio::time::sleep(Duration::from_millis(50)).await;
        let held = stream.parts.recv().await.unwrap().update;
        assert_eq!(held.position, Some(Position { epoch: 1, revision: 500 }));

        let sent = Instant::now();
        sender.send(upsert("a", "1", 501));
        let update = stream.parts.recv().await.unwrap().update;
        assert_eq!(Instant::now(), sent);
        assert_eq!((update.upserts.len(), update.position), (1, Some(Position { epoch: 1, revision: 501 })));
    }
}
