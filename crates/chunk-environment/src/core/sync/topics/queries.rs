//! The `queries` topic: app queries the client names by key, kept current through one shared backend group.

use super::super::{NAME_BYTES, errors, position, streams::Sender};
use chunk_backend::{Backend, Call, GroupSubscription};
use chunk_proto::sync::v1::{Entry, Error, Update, entry::State, error::Code};
use chunk_store::Revision;
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Arc};
use tokio_util::sync::CancellationToken;

/// The most queries one subscription names, as one backend group.
const QUERIES: usize = 16;

/// One key of the topic's arguments, a JSON object mapping each key to the query that fills it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    function: String,
    #[serde(default)]
    arguments: serde_json::Value,
}

pub(in super::super) struct Queries {
    keys: Vec<String>,
    group: GroupSubscription,
    stream: String,
    epoch: u64,
}

impl Queries {
    /// Subscribes to the queries `arguments` names, each called as `caller` in `deployment`.
    pub async fn open(
        backend: &Backend,
        deployment: &chunk_js::DeploymentId,
        caller: &chunk_js::Json,
        arguments: &[u8],
        stream: String,
        epoch: u64,
    ) -> Result<Self, Error> {
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
        Ok(Self { keys, group, stream, epoch })
    }

    /// Sends a snapshot, then each key whose result changed since it was last sent, and position-only updates when
    /// the results still hold at a later commit. A slow client gets the latest state, never an intermediate one.
    pub async fn run(mut self, sender: Sender, stop: CancellationToken) {
        let mut sent: Vec<Option<Result<Arc<str>, Error>>> = vec![None; self.keys.len()];
        let mut revision = Revision(0);
        let mut first = true;
        loop {
            let next = async { if sender.ready().await { Some(self.group.next_revision().await) } else { None } };
            let update = tokio::select! {
                () = stop.cancelled() => {
                    return sender.fail(errors::error(Code::Unavailable, "core is stopping"));
                }
                () = sender.closed() => return,
                update = next => update,
            };
            let update = match update {
                None => return,
                Some(Err(failure)) => return sender.fail(errors::backend(&failure)),
                Some(Ok(update)) => update,
            };
            let mut upserts = Vec::new();
            for ((key, last), result) in self.keys.iter().zip(&mut sent).zip(update.results) {
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
            if !first && upserts.is_empty() && update.revision <= revision {
                continue;
            }
            revision = revision.max(update.revision);
            let update = Update {
                position: position(self.epoch, revision),
                snapshot: first,
                upserts,
                stream: if first { self.stream.clone() } else { String::new() },
                ..Update::default()
            };
            if !sender.send(update).await {
                return;
            }
            first = false;
        }
    }
}
