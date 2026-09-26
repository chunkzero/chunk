//! The `queries` topic: app queries the client names by key, kept current through one shared backend group.

use super::{
    super::{NAME_BYTES, caller::Grant, errors, position, streams::Sender},
    Context,
};
use chunk_backend::{Backend, Call, GroupSubscription, GroupUpdate};
use chunk_proto::sync::v1::{Entry, Error, Update, entry::State, error::Code};
use chunk_store::Revision;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

/// The most queries one subscription names, as one backend group.
const QUERIES: usize = 16;
/// Least time between position-only updates while none of the credential's own mutations is pending.
const IDLE: Duration = Duration::from_secs(1);

/// One key of the topic's arguments, a JSON object mapping each key to the query that fills it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    function: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

/// Subscribes to the queries `arguments` names, each called as `caller` in `deployment`, returning their keys.
pub(super) async fn subscribe(
    backend: &Backend,
    deployment: &chunk_js::DeploymentId,
    caller: &chunk_js::Json,
    arguments: &[u8],
) -> Result<(Vec<String>, GroupSubscription), Error> {
    let queries: BTreeMap<String, Query> = serde_json::from_slice(arguments)
        .map_err(|_| errors::invalid("queries arguments map each key to {\"function\", \"arguments\"}"))?;
    if queries.is_empty() || queries.len() > QUERIES {
        return Err(errors::invalid(format!("name between 1 and {QUERIES} queries")));
    }
    if queries.keys().any(|key| key.len() > NAME_BYTES) {
        return Err(errors::invalid("a key exceeds 512 bytes"));
    }
    let (keys, calls) = queries
        .into_iter()
        .map(|(key, query)| {
            let call = Call {
                deployment: deployment.clone(),
                function: query.function,
                arguments: query.arguments.into(),
                caller: caller.clone(),
            };
            (key, call)
        })
        .unzip();
    let group = backend.subscribe_group(calls).await.map_err(|failure| errors::backend(&failure))?;
    Ok((keys, group))
}

pub(in super::super) struct Queries {
    keys: Vec<String>,
    group: GroupSubscription,
    context: Context,
}

enum Event {
    Results(GroupUpdate),
    Control,
    Nudge,
    Advance,
}

impl Queries {
    pub fn new(keys: Vec<String>, group: GroupSubscription, context: Context) -> Self {
        Self { keys, group, context }
    }

    /// Sends a snapshot, then each key whose result changed since it was last sent, and position-only updates when
    /// the results still hold at a later commit: promptly up to the credential's own mutations, otherwise at most
    /// once per [`IDLE`]. Never waits for the client, so the grant is rechecked on each control change and before
    /// each update however slowly it reads. Ends once the grant lapses.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { keys, mut group, context: Context { id, grant, mut nudges, epoch } } = self;
        let mut progress = group.progress();
        let mut changes = grant.changes();
        let mut sent: Vec<Option<Result<Arc<str>, Error>>> = vec![None; keys.len()];
        let mut revision = Revision(0);
        // The version of the results sent, once the snapshot is.
        let mut version = None;
        // A commit of the credential's own mutations the stream has yet to reach.
        let mut pending = None;
        let mut checked = Instant::now();
        loop {
            let advance = async {
                if pending.is_some() {
                    progress.changed().await;
                } else {
                    progress.durable_after(revision).await;
                    tokio::time::sleep_until(checked + IDLE).await;
                }
            };
            let event = tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = sender.closed() => return,
                update = group.next() => match update {
                    Err(failure) => return sender.fail(errors::backend(&failure)),
                    Ok(update) => Event::Results(update),
                },
                () = changed(&mut changes) => Event::Control,
                () = changed(&mut nudges), if version.is_some() => Event::Nudge,
                () = advance, if version.is_some() => Event::Advance,
            };
            match event {
                Event::Control => {
                    if let Err(error) = grant.check() {
                        return sender.fail(error);
                    }
                    continue;
                }
                Event::Results(update) => {
                    let mut upserts = Vec::new();
                    for ((key, last), result) in keys.iter().zip(&mut sent).zip(update.results) {
                        let result = result.map_err(|failure| errors::backend(&failure));
                        if last.as_ref() != Some(&result) {
                            let state = match &result {
                                Ok(json) => State::Value(json.as_bytes().to_vec()),
                                Err(error) => State::Error(error.clone()),
                            };
                            upserts.push(Entry { key: key.clone(), state: Some(state) });
                            *last = Some(result);
                        }
                    }
                    let first = version.replace(update.version).is_none();
                    if first || !upserts.is_empty() || update.revision > revision {
                        revision = revision.max(update.revision);
                        let update = Update {
                            position: position(epoch, revision),
                            snapshot: first,
                            upserts,
                            stream: if first { id.clone() } else { String::new() },
                            ..Update::default()
                        };
                        if !send(&grant, &sender, update) {
                            return;
                        }
                    }
                    if pending.is_none() {
                        continue;
                    }
                }
                Event::Nudge => pending = pending.max(Some(*nudges.borrow_and_update())),
                Event::Advance => checked = Instant::now(),
            }
            let last = version.and_then(|version| progress.holds(version)).filter(|last| *last > revision);
            if let Some(last) = last {
                revision = last;
                if !send(&grant, &sender, Update { position: position(epoch, revision), ..Update::default() }) {
                    return;
                }
            }
            if pending.is_some_and(|pending| revision >= pending) {
                pending = None;
            }
        }
    }
}

/// Sends `update` if the grant still holds, and otherwise ends the stream with why it lapsed. Returns whether the
/// stream continues.
fn send(grant: &Grant, sender: &Sender, update: Update) -> bool {
    if let Err(error) = grant.check() {
        sender.fail(error);
        return false;
    }
    sender.send(update);
    true
}

/// Waits for `receiver` to change, or forever once its sender is gone.
async fn changed<T>(receiver: &mut watch::Receiver<T>) {
    if receiver.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}
