//! The `jvm/<host>` topic: the sessions, deliveries and session methods control wants one JVM to run, and whether it
//! should stop, sent as snapshots.
//! Only that JVM's credential may subscribe. Control decides which of the JVM's streams is current, and a newer
//! stream ends older ones.

use super::super::{
    SyncService,
    auth::{Class, Credentials, Principal},
    errors,
    streams::Sender,
};
use chunk_control::jvm::Topic;
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update, error::Code};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Jvm {
    topic: Topic,
    first: Update,
    credentials: Arc<Credentials>,
    principal: Principal,
}

/// Opens the JVM topic `request` names under a new stream ID, which starts from a snapshot whatever its cursor, and
/// ends the JVM's earlier stream.
pub(super) fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Jvm, Error> {
    let host = request.topic.strip_prefix("jvm/").unwrap_or_default();
    if principal.class != (Class::Jvm { host: host.to_owned() }) {
        return Err(errors::denied("only the host's own JVM follows its topic"));
    }
    if !request.arguments.is_empty() || !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the JVM topic takes no arguments, deployment or caller"));
    }
    let stream = service.streams.id(request, &principal.credential);
    let (topic, first) = Topic::open(&service.control, host, &stream).map_err(|failure| errors::control(&failure))?;
    Ok(Jvm { topic, first: Update { stream, ..first }, credentials: service.credentials.clone(), principal })
}

impl Jvm {
    /// Sends the first snapshot, then one whenever control wants something else of the JVM, until the client leaves,
    /// core stops, the JVM's process stops or control ends the stream.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut topic, first, credentials, principal } = self;
        let ended = topic.ended();
        let mut next = Ok(Some(first));
        loop {
            if credentials.class(&principal.credential).as_ref() != Some(&principal.class) {
                return sender.fail(errors::error(Code::Stopped, "the credential's process stopped"));
            }
            match next {
                Ok(Some(update)) => sender.send(update),
                Ok(None) => {}
                Err(failure) => return sender.fail(errors::control(&failure)),
            }
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = ended.cancelled() => {
                    return sender.fail(errors::error(Code::Stopped, "a newer stream superseded this one, or the JVM stopped"));
                }
                () = sender.closed() => return,
                () = topic.changed() => {}
            }
            next = topic.update();
        }
    }
}
