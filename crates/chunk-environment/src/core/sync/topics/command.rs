//! The `command/<op>` topic: the packet effects the command started under prepared operation ID `<op>` waits for its
//! gateway to render, sent as snapshots, then its outcome. Only the gateway credential that started the command may
//! follow it, on its current gateway stream, through one subscription at a time, running or finished, and the command
//! is cancelled once nothing follows it. A subscription under an unused ID reserves it for its gateway credential: it
//! sends an empty snapshot at once, then waits for the command to start, and closing it first cancels the command. The
//! subscription's request is charged against the backend's request memory, with room for the record of a command it
//! closes before it started, until its topic ends or that record is forgotten, and each snapshot until the client took
//! it.

use super::super::{
    SyncService,
    auth::{Class, Principal},
    caller::Grant,
    errors,
    runs::{Pending, RECORD, Run, Subscription, outcome},
    streams::Sender,
};
use chunk_backend::{ActionStatus, Backend, CommandIdentity};
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
    backend: Backend,
}

pub(super) async fn open(
    service: &SyncService,
    principal: Principal,
    request: &SubscribeRequest,
) -> Result<Command, Error> {
    let backend = service.app.backend();
    let charge = backend.charge_request(request.encoded_len() + RECORD + principal.credential.len());
    let charge = charge.map_err(|failure| errors::backend(&failure))?;
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
    let found = match service.runs.get(operation) {
        Some(_) => None,
        None => retained(backend, id, &principal.credential).await?,
    };
    let (pending, subscription) = service.runs.follow(
        operation,
        found,
        &principal.credential,
        &superseded,
        service.stop.child_token(),
        charge,
    )?;
    Ok(Command {
        pending,
        subscription,
        stream: service.streams.id(request, &principal.credential),
        superseded,
        grant: Grant::new(service.credentials.clone(), principal, "", None),
        backend: backend.clone(),
    })
}

/// The command `credential` started under `id` that no longer runs, as the backend retains it, or none if `id` is
/// unused.
async fn retained(backend: &Backend, id: chunk_backend::ActionId, credential: &str) -> Result<Option<Arc<Run>>, Error> {
    let identity = backend.command_identity(id, credential, None).await;
    match identity.map_err(|failure| errors::backend(&failure))? {
        CommandIdentity::Started(ActionStatus::Finished(result)) => {
            Ok(Some(Run::finished(credential, outcome(backend, result)?)))
        }
        CommandIdentity::Started(ActionStatus::Running) => Err(errors::error(Code::Unavailable, "retry the command")),
        CommandIdentity::Unused => Ok(None),
        CommandIdentity::Foreign => Err(errors::denied("another gateway ran this command")),
        CommandIdentity::Other => Err(errors::error(Code::OperationMismatch, "the operation ID ran an action or hook")),
    }
}

impl Command {
    /// Sends an empty snapshot while the command has yet to start, then a snapshot of the pending effects whenever they
    /// change, then the command's outcome, until the client took it or leaves, core stops, the credential lapses, as
    /// when it's revoked, the backend has no room for a snapshot, or the subscription itself or, once the command
    /// started, the gateway stream it names is superseded. A subscription that ends for its command or the backend
    /// releases what its client has yet to take at once.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut pending, subscription, stream, superseded, grant, backend } = self;
        let (mut first, mut finished) = (Some(stream), false);
        loop {
            let (update, waits) = {
                let current = pending.borrow_and_update();
                if subscription.replaced(&current) {
                    return sender.end(errors::error(Code::Stopped, "a newer subscription follows the command"));
                }
                let waits = current.waits();
                let update = if waits {
                    // An empty snapshot confirms the reservation before the command starts.
                    first.is_some().then(|| Ok((Update { snapshot: true, ..Update::default() }, false)))
                } else {
                    (!finished).then(|| current.snapshot())
                };
                (update, waits)
            };
            if let Some(snapshot) = update {
                let (update, last) = match snapshot {
                    Ok(snapshot) => snapshot,
                    Err(error) => return sender.end(error),
                };
                if let Err(error) = grant.check() {
                    return sender.fail(error);
                }
                let update = Update { stream: first.take().unwrap_or_default(), ..update };
                match backend.charge_request(update.encoded_len()) {
                    Ok(charge) => sender.send_snapshot(update, charge),
                    Err(failure) => return sender.end(errors::backend(&failure)),
                }
                if last {
                    finished = true;
                    sender.finish();
                }
            }
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = sender.closed() => return,
                () = superseded.cancelled(), if !waits => {
                    subscription.superseded();
                    return sender.end(errors::error(Code::Stopped, "a newer gateway stream superseded the one named"));
                }
                error = grant.lapsed() => return sender.fail(error),
                changed = pending.changed() => if changed.is_err() { return },
            }
        }
    }
}
