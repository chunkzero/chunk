//! The `command/<op>` topic: the packet effects the command under prepared operation ID `<op>` waits for its gateway
//! to render, sent as snapshots. Only the gateway running the command may subscribe.

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
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update, error::Code};
use std::sync::Arc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Command {
    /// Keeps the command open while the stream follows it.
    _run: Arc<Run>,
    pending: watch::Receiver<Pending>,
    stream: String,
    grant: Grant,
}

pub(super) fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Command, Error> {
    let operation = request.topic.strip_prefix("command/").unwrap_or_default();
    let Class::Gateway { id } = &principal.class else {
        return Err(errors::denied("only a gateway follows its commands"));
    };
    if !request.arguments.is_empty() || !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("the command topic takes no arguments, deployment or caller"));
    }
    super::super::app::prepared(operation)?;
    let run = service.runs.open(operation, id)?;
    Ok(Command {
        pending: run.follow(),
        _run: run,
        stream: service.streams.id(request, &principal.credential),
        grant: Grant::new(service.credentials.clone(), principal, "", None),
    })
}

impl Command {
    /// Sends a snapshot of the pending effects whenever they change, until the client leaves, core stops, the
    /// credential lapses or the command finishes.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { _run, mut pending, stream, grant } = self;
        let mut first = Some(stream);
        loop {
            let (update, finished) = {
                let pending = pending.borrow_and_update();
                (pending.snapshot(), pending.finished())
            };
            if finished {
                return sender.fail(errors::error(Code::Stopped, "the command finished"));
            }
            if !send(&grant, &sender, Update { stream: first.take().unwrap_or_default(), ..update }) {
                return;
            }
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = sender.closed() => return,
                changed = pending.changed() => if changed.is_err() { return },
            }
        }
    }
}
