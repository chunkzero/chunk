//! The `command/<op>` topic: the packet effects the command under prepared operation ID `<op>` waits for its gateway
//! to render, sent as snapshots. Only the gateway credential that started the command may follow it, on its current
//! gateway stream.

use super::{
    super::{
        SyncService,
        auth::{Class, Principal},
        caller::Grant,
        errors,
        runs::{Pending, Run},
        streams::Sender,
    },
    send,
};
use chunk_proto::sync::v1::{CommandSubscription, Error, SubscribeRequest, Update, error::Code};
use prost::Message;
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Command {
    /// Keeps the command open while the stream follows it.
    _run: Arc<Run>,
    pending: watch::Receiver<Pending>,
    stream: String,
    credential: String,
    /// Cancelled once the gateway stream the subscription names is superseded.
    superseded: CancellationToken,
    grant: Grant,
}

pub(super) fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Command, Error> {
    let operation = request.topic.strip_prefix("command/").unwrap_or_default();
    if !matches!(principal.class, Class::Gateway { .. }) {
        return Err(errors::denied("only a gateway follows its commands"));
    }
    if !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the command topic takes no deployment or caller"));
    }
    let arguments = CommandSubscription::decode(request.arguments.as_slice());
    let arguments = arguments.map_err(|_| errors::invalid("arguments are not a CommandSubscription"))?;
    super::super::app::prepared(operation)?;
    let superseded = service.fences.follow(&arguments.stream, &principal.credential)?;
    let run = service.runs.open(operation);
    run.permits(&principal.credential)?;
    Ok(Command {
        pending: run.follow(),
        _run: run,
        stream: service.streams.id(request, &principal.credential),
        credential: principal.credential.clone(),
        superseded,
        grant: Grant::new(service.credentials.clone(), principal, "", None),
    })
}

impl Command {
    /// Sends a snapshot of the pending effects whenever they change, until the client leaves, core stops, the
    /// credential lapses, its gateway stream is superseded, another credential starts the command or it finishes.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { _run, mut pending, stream, credential, superseded, grant } = self;
        let mut first = Some(stream);
        loop {
            let (update, permitted, finished) = {
                let pending = pending.borrow_and_update();
                (pending.snapshot(), pending.permits(&credential), pending.finished())
            };
            if let Err(error) = permitted {
                return sender.fail(error);
            }
            if finished {
                return sender.fail(errors::error(Code::Stopped, "the command finished"));
            }
            if !send(&grant, &sender, Update { stream: first.take().unwrap_or_default(), ..update }) {
                return;
            }
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = sender.closed() => return,
                () = superseded.cancelled() => {
                    return sender.fail(errors::error(Code::Stopped, "a newer gateway stream superseded the one named"));
                }
                changed = pending.changed() => if changed.is_err() { return },
            }
        }
    }
}
