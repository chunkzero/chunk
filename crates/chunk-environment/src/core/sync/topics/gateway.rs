//! The `gateway/<id>` topic: the claims one gateway holds, followed through control's commits. Only that gateway's
//! credential may subscribe, and a newer stream supersedes older ones.

use super::{
    super::{SyncService, auth::Class, auth::Principal, caller::Grant, errors, streams::Sender},
    changed, send,
};
use chunk_control::{Control, Generation, gateway::Topic};
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update, error::Code};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Gateway {
    topic: Topic,
    first: Update,
    control: Arc<Control>,
    positions: watch::Receiver<Generation>,
    grant: Grant,
    /// Cancelled once a newer stream for this gateway opens.
    superseded: CancellationToken,
}

/// Opens the gateway topic `request` names, resuming after its cursor when that names this stream in the current
/// epoch, and supersedes the gateway's earlier stream.
pub(super) fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Gateway, Error> {
    let id = request.topic.strip_prefix("gateway/").unwrap_or_default();
    if principal.class != (Class::Gateway { id: id.to_owned() }) {
        return Err(errors::denied("only the gateway's own credential follows its topic"));
    }
    if !request.arguments.is_empty() || !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the gateway topic takes no arguments, deployment or caller"));
    }
    let stream = service.streams.id(request, &principal.credential);
    let after = request.after.as_ref().filter(|after| after.stream == stream).and_then(|after| after.position);
    let after = after
        .filter(|position| position.epoch == service.epoch)
        .map(|position| Generation { epoch: position.epoch, revision: position.revision });
    let positions = service.control.subscribe();
    let (topic, first) = Topic::open(&service.control, id, after).map_err(|failure| errors::control(&failure))?;
    Ok(Gateway {
        topic,
        first: Update { stream, ..first },
        control: service.control.clone(),
        positions,
        grant: Grant::new(service.credentials.clone(), principal, "", None),
        superseded: service.fences.fence(&request.topic),
    })
}

impl Gateway {
    /// Sends the first update, then one for each control commit, until the client leaves, core stops, the credential
    /// lapses or a newer stream supersedes this one.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut topic, first, control, mut positions, grant, superseded } = self;
        if !send(&grant, &sender, first) {
            return;
        }
        loop {
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = superseded.cancelled() => {
                    return sender.fail(errors::error(Code::Stopped, "a newer stream for this gateway superseded this one"));
                }
                () = sender.closed() => return,
                () = changed(&mut positions) => {}
            }
            match topic.next(&control) {
                Ok(None) => {}
                Ok(Some(update)) => {
                    if !send(&grant, &sender, update) {
                        return;
                    }
                }
                Err(failure) => return sender.fail(errors::control(&failure)),
            }
        }
    }
}
