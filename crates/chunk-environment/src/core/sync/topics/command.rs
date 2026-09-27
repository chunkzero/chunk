//! The `command/<op>` topic: the packet effects the command started under prepared operation ID `<op>` waits for its
//! gateway to render, sent as snapshots, then its outcome. Only the gateway credential that started the command may
//! follow it, on its current gateway stream, through one subscription at a time, running or finished, and the command
//! is cancelled once nothing follows it.

use super::{
    super::{
        SyncService,
        auth::{Class, Principal},
        caller::Grant,
        errors,
        runs::{Pending, Run, Subscription, outcome},
        streams::Sender,
    },
    send,
};
use chunk_backend::{ActionStatus, CommandIdentity};
use chunk_proto::sync::v1::{CommandSubscription, Error, SubscribeRequest, Update, error::Code};
use prost::Message;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Command {
    pending: watch::Receiver<Pending>,
    subscription: Subscription,
    stream: String,
    /// Cancelled once the gateway stream the subscription names is superseded.
    superseded: CancellationToken,
    grant: Grant,
}

pub(super) async fn open(
    service: &SyncService,
    principal: Principal,
    request: &SubscribeRequest,
) -> Result<Command, Error> {
    let operation = request.topic.strip_prefix("command/").unwrap_or_default();
    if !matches!(principal.class, Class::Gateway { .. }) {
        return Err(errors::denied("only a gateway follows its commands"));
    }
    if !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the command topic takes no deployment or caller"));
    }
    let arguments = CommandSubscription::decode(request.arguments.as_slice());
    let arguments = arguments.map_err(|_| errors::invalid("arguments are not a CommandSubscription"))?;
    let id = super::super::app::prepared(operation)?;
    let superseded = service.fences.follow(&arguments.stream, &principal.credential)?;
    let running = match service.runs.get(operation) {
        Some(run) => {
            run.permits(&principal.credential, None)?;
            run.started().await?.then_some(run)
        }
        None => None,
    };
    let run = match running {
        Some(run) => run,
        None => retained(service, id, &principal.credential).await?,
    };
    let (pending, subscription) = service.runs.follow(operation, run, &principal.credential, &superseded)?;
    Ok(Command {
        pending,
        subscription,
        stream: service.streams.id(request, &principal.credential),
        superseded,
        grant: Grant::new(service.credentials.clone(), principal, "", None),
    })
}

/// The command `credential` started under `id` that no longer runs, as the backend retains it.
async fn retained(service: &SyncService, id: chunk_backend::ActionId, credential: &str) -> Result<Arc<Run>, Error> {
    let identity = service.app.backend().command_identity(id, credential, None).await;
    match identity.map_err(|failure| errors::backend(&failure))? {
        CommandIdentity::Started(ActionStatus::Finished(result)) => Ok(Run::finished(credential, outcome(result))),
        CommandIdentity::Started(ActionStatus::Running) => Err(errors::error(Code::Unavailable, "retry the command")),
        CommandIdentity::Unused => Err(errors::invalid("no command started under this operation ID")),
        CommandIdentity::Foreign => Err(errors::denied("another gateway ran this command")),
        CommandIdentity::Other => Err(errors::error(Code::OperationMismatch, "the operation ID ran an action or hook")),
    }
}

impl Command {
    /// Sends a snapshot of the pending effects whenever they change, then the command's outcome, until the client
    /// took it or leaves, core stops, the credential lapses, or the gateway stream it names or the subscription itself
    /// is superseded. A superseded subscription releases what its client has yet to take at once.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut pending, subscription, stream, superseded, grant } = self;
        let (mut first, mut finished) = (Some(stream), false);
        loop {
            let update = {
                let current = pending.borrow_and_update();
                if subscription.replaced(&current) {
                    return sender.end(errors::error(Code::Stopped, "a newer subscription follows the command"));
                }
                (!finished).then(|| current.snapshot())
            };
            if let Some((update, last)) = update {
                if !send(&grant, &sender, Update { stream: first.take().unwrap_or_default(), ..update }) {
                    return;
                }
                if last {
                    finished = true;
                    sender.finish();
                }
            }
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = sender.closed() => return,
                () = superseded.cancelled() => {
                    subscription.superseded();
                    return sender.fail(errors::error(Code::Stopped, "a newer gateway stream superseded the one named"));
                }
                changed = pending.changed() => if changed.is_err() { return },
            }
        }
    }
}
