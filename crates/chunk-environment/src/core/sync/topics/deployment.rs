//! The `deployment` topic: the deployment gateways route new logins with, sent as a snapshot whenever control's
//! current release changes. Any gateway's credential may subscribe, and many gateways follow it at once, so it isn't
//! fenced.

use super::{
    super::{SyncService, auth::Class, auth::Principal, caller::Grant, errors, streams::Sender},
    changed, send,
};
use chunk_control::{Control, Generation, gateway};
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update, error::Code};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Deployment {
    topic: gateway::Deployment,
    first: Update,
    control: Arc<Control>,
    positions: watch::Receiver<Generation>,
    grant: Grant,
}

/// Opens the topic under a new stream ID. Its first update is a snapshot whatever its cursor.
pub(super) fn open(
    service: &SyncService,
    principal: Principal,
    request: &SubscribeRequest,
) -> Result<Deployment, Error> {
    if !matches!(principal.class, Class::Gateway { .. }) {
        return Err(errors::denied("only gateways follow the deployment topic"));
    }
    if !request.arguments.is_empty() || !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the deployment topic takes no arguments, deployment or caller"));
    }
    let positions = service.control.subscribe();
    let (topic, first) = gateway::Deployment::open(&service.control).map_err(|failure| errors::control(&failure))?;
    let stream = service.streams.id(request, &principal.credential);
    Ok(Deployment {
        topic,
        first: Update { stream, ..first },
        control: service.control.clone(),
        positions,
        grant: Grant::new(service.credentials.clone(), principal, "", None),
    })
}

impl Deployment {
    /// Sends the first snapshot, then one whenever the current release changes, until the client leaves, core stops,
    /// or the credential lapses, as when it's revoked.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut topic, first, control, mut positions, grant } = self;
        if !send(&grant, &sender, first) {
            return;
        }
        loop {
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
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
