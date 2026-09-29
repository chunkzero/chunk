//! The `gateway/<id>` topic: the claims one gateway holds, followed through control's commits. Only that gateway's
//! credential may subscribe, and a newer stream supersedes older ones. The process instance each subscription names
//! owns the topic, as gateway.proto describes.

use super::{
    super::{
        SyncService, auth::Class, auth::Principal, caller::Grant, errors, liveness::Live, streams::Fenced,
        streams::Sender,
    },
    changed, send,
};
use chunk_control::{Control, Generation, gateway::Topic};
use chunk_proto::sync::v1::{Error, GatewayArguments, SubscribeRequest, Update, error::Code};
use prost::Message;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Gateway {
    topic: Topic,
    first: Update,
    control: Arc<Control>,
    positions: watch::Receiver<Generation>,
    grant: Grant,
    fenced: Fenced,
    /// Counts the stream as live until it ends.
    live: Live,
}

/// Opens the gateway topic `request` names under a new stream ID, resuming after its cursor when that names a stream
/// of the same scope in the current epoch. The stream is fenced before the first update is read.
pub(super) fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Gateway, Error> {
    let id = request.topic.strip_prefix("gateway/").unwrap_or_default();
    if principal.class != (Class::Gateway { id: id.to_owned() }) {
        return Err(errors::denied("only the gateway's own credential follows its topic"));
    }
    if !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the gateway topic takes no deployment or caller"));
    }
    let instance = GatewayArguments::decode(request.arguments.as_slice()).map(|arguments| arguments.instance);
    let instance = instance.ok().filter(|instance| (1..=64).contains(&instance.len()));
    let instance = instance.ok_or_else(|| errors::invalid("the gateway topic takes an instance of 1 to 64 bytes"))?;
    let credential = &principal.credential;
    let stream = service.streams.id(request, credential);
    let fenced = service.fences.fence(&request.topic, &instance, &stream, credential)?;
    let after = request.after.as_ref().filter(|after| service.streams.verify(&after.stream, request, credential));
    let after = after
        .and_then(|after| after.position)
        .filter(|position| position.epoch == service.epoch)
        .map(|position| Generation { epoch: position.epoch, revision: position.revision });
    let positions = service.control.subscribe();
    let (topic, first) = Topic::open(&service.control, id, after).map_err(|failure| errors::control(&failure))?;
    let live = service.credentials.gateways.liveness.open(id);
    Ok(Gateway {
        topic,
        first: Update { stream, ..first },
        control: service.control.clone(),
        positions,
        grant: Grant::new(service.credentials.clone(), principal, "", None),
        fenced,
        live,
    })
}

impl Gateway {
    /// Sends the first update, then one for each control commit, until the client leaves, core stops, the credential
    /// lapses, as when it's revoked, or a newer stream supersedes this one.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut topic, first, control, mut positions, grant, fenced, live: _live } = self;
        if !send(&grant, &sender, first) {
            return;
        }
        loop {
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = fenced.superseded.cancelled() => {
                    return sender.fail(if fenced.retired.is_cancelled() {
                        errors::error(Code::Superseded, "a later process under this gateway's ID took its topic over")
                    } else {
                        errors::error(Code::Stopped, "a newer stream for this gateway superseded this one")
                    });
                }
                error = grant.lapsed() => return sender.fail(error),
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
