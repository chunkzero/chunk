//! The operator's topics, `nodes` and `players`, which only the CLI's credential may follow. Each stream starts from a
//! snapshot whatever its cursor, and isn't fenced.

use super::super::{
    SyncService,
    auth::{Class, Principal},
    errors,
    streams::Sender,
};
use chunk_control::operator::{Nodes, Players};
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update, error::Code};
use tokio_util::sync::CancellationToken;

pub(in super::super) struct Operator {
    view: View,
    first: Update,
}

enum View {
    Nodes(Nodes),
    Players(Players),
}

/// Opens the operator topic `request` names under a new stream ID.
pub(super) fn open(
    service: &SyncService,
    principal: &Principal,
    request: &SubscribeRequest,
) -> Result<Operator, Error> {
    if principal.class != Class::Cli {
        return Err(errors::denied("only the operator follows this topic"));
    }
    if !request.arguments.is_empty() || !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("an operator topic takes no arguments, deployment or caller"));
    }
    let failed = |failure| errors::control(&failure);
    let (view, first) = if request.topic == "nodes" {
        let (nodes, first) = Nodes::open(&service.control).map_err(failed)?;
        (View::Nodes(nodes), first)
    } else {
        let (players, first) = Players::open(&service.control).map_err(failed)?;
        (View::Players(players), first)
    };
    let stream = service.streams.id(request, &principal.credential);
    Ok(Operator { view, first: Update { stream, ..first } })
}

impl Operator {
    /// Sends the first snapshot, then each change, until the client leaves or core stops.
    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        let Self { mut view, first } = self;
        sender.send(first);
        loop {
            tokio::select! {
                () = stop.cancelled() => return sender.fail(errors::error(Code::Unavailable, "core is stopping")),
                () = sender.closed() => return,
                () = view.changed() => {}
            }
            let update = match &mut view {
                View::Nodes(nodes) => nodes.update(),
                View::Players(players) => players.update(),
            };
            match update {
                Ok(Some(update)) => sender.send(update),
                Ok(None) => {}
                Err(failure) => return sender.fail(errors::control(&failure)),
            }
        }
    }
}

impl View {
    async fn changed(&mut self) {
        match self {
            Self::Nodes(nodes) => nodes.changed().await,
            Self::Players(players) => players.changed().await,
        }
    }
}
