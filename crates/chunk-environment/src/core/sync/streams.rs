//! Subscription streams: their IDs, message splitting, a sender that only ever sends the latest state, and nudges
//! after a credential's own writes.

use super::{MESSAGE_BYTES, errors};
use chunk_proto::sync::v1::{Error, Position, SubscribeRequest, Update, entry::State};
use chunk_store::Revision;
use prost::Message;
use sha2::{Digest, Sha256};
use std::{collections::HashMap, fmt::Write, sync::Mutex};
use tokio::sync::{mpsc, watch};
use tonic::Status;

/// Room left in each message for its position, flags and stream ID.
const HEADER_BYTES: usize = 2048;

/// Keys stream IDs for this process only, so a stream from before a restart never resumes.
pub(super) struct StreamKey([u8; 32]);

impl StreamKey {
    pub fn new() -> Self {
        let mut key = [0; 32];
        key[..16].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        key[16..].copy_from_slice(uuid::Uuid::new_v4().as_bytes());
        Self(key)
    }

    /// An HMAC-SHA256 over the subscription's topic, arguments, deployment, caller and credential.
    pub fn id(&self, request: &SubscribeRequest, credential: &str) -> String {
        let caller = request.caller.as_ref();
        let fields = [
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
        let mut block = [0; 64];
        block[..32].copy_from_slice(&self.0);
        let inner = Sha256::new().chain_update(block.map(|byte| byte ^ 0x36)).chain_update(&message).finalize();
        let mac = Sha256::new().chain_update(block.map(|byte| byte ^ 0x5c)).chain_update(inner).finalize();
        mac.iter().fold(String::with_capacity(64), |mut id, byte| {
            let _ = write!(id, "{byte:02x}");
            id
        })
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

pub(super) type Stream = mpsc::Receiver<Result<Update, Status>>;

/// Holds at most one update in flight, so a subscriber waits for room before reading the state it sends.
pub(super) struct Sender(mpsc::Sender<Result<Update, Status>>);

pub(super) fn channel() -> (Sender, Stream) {
    let (sender, receiver) = mpsc::channel(1);
    (Sender(sender), receiver)
}

impl Sender {
    /// Waits until the client can take another update. Returns `false` once it went away.
    pub async fn ready(&self) -> bool {
        self.0.reserve().await.is_ok()
    }

    pub async fn closed(&self) {
        self.0.closed().await;
    }

    /// Sends `update`, split into continued updates that each fit in a message. Returns `false` once the client
    /// went away.
    pub async fn send(&self, update: Update) -> bool {
        for part in split(update) {
            if self.0.send(Ok(part)).await.is_err() {
                return false;
            }
        }
        true
    }

    /// Sends `error` as the stream's final update, unless the client is too far behind to take it; either way the
    /// stream ends once this sender drops.
    pub fn fail(&self, error: Error) {
        let _ = self.0.try_send(Ok(Update { error: Some(error), ..Update::default() }));
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
