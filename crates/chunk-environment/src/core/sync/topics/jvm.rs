//! The `jvm/<host>` topic: the sessions control wants one JVM to run, and whether it should stop, sent as snapshots.
//! Only that JVM's credential may subscribe, and a newer stream supersedes older ones.

use super::{
    super::{
        SyncService,
        auth::{Class, Credentials, Principal},
        errors,
        streams::Sender,
    },
    changed,
};
use chunk_control::{Generation, jvm::Topic};
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update, error::Code};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Jvm {
    topic: Topic,
    first: Update,
    positions: watch::Receiver<Generation>,
    credentials: Arc<Credentials>,
    principal: Principal,
    /// Cancelled once a newer stream for this JVM opens.
    superseded: CancellationToken,
}

/// Opens the JVM topic `request` names under a new stream ID, which starts from a snapshot whatever its cursor, and
/// supersedes the JVM's earlier stream.
pub(super) fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Jvm, Error> {
    let host = request.topic.strip_prefix("jvm/").unwrap_or_default();
    if principal.class != (Class::Jvm { host: host.to_owned() }) {
        return Err(errors::denied("only the host's own JVM follows its topic"));
    }
    if !request.arguments.is_empty() || !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the JVM topic takes no arguments, deployment or caller"));
    }
    let positions = service.control.subscribe();
    let stream = service.streams.id(request, &principal.credential);
    let (topic, first) = Topic::open(&service.control, host, &stream).map_err(|failure| errors::control(&failure))?;
    let superseded = service.fences.fence(&request.topic, &stream, &principal.credential);
    Ok(Jvm {
        topic,
        first: Update { stream, ..first },
        positions,
        credentials: service.credentials.clone(),
        principal,
        superseded,
    })
}

impl Jvm {
    /// Sends the first snapshot, then one whenever control wants something else of the JVM, until the client leaves,
    /// core stops, the JVM's process stops or a newer stream supersedes this one.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut topic, first, mut positions, credentials, principal, superseded } = self;
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
                () = superseded.cancelled() => {
                    return sender.fail(errors::error(Code::Stopped, "a newer stream for this JVM superseded this one"));
                }
                () = sender.closed() => return,
                () = changed(&mut positions) => {}
            }
            next = topic.update();
        }
    }
}
