//! Subscription topics, by name.

mod command;
mod gateway;
mod jvm;
mod operator;
mod queries;

use super::{SyncService, auth::Principal, caller::Grant, errors, streams::Sender};
use chunk_proto::sync::v1::{Error, SubscribeRequest, Update};
use chunk_store::Revision;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

pub(super) enum Topic {
    Queries(queries::Queries),
    Command(command::Command),
    Gateway(gateway::Gateway),
    Jvm(jvm::Jvm),
    Operator(operator::Operator),
}

/// What every stream carries besides its topic's state.
pub(super) struct Context {
    /// Names the stream in its first update.
    pub id: String,
    /// Checked before each update; the stream ends once it lapses.
    pub grant: Grant,
    /// The latest commit of the credential's own mutations, which the stream catches up to promptly.
    pub nudges: watch::Receiver<Revision>,
    pub epoch: u64,
}

impl Topic {
    /// Checks and subscribes to the topic `request` names.
    pub async fn open(service: &SyncService, principal: Principal, request: &SubscribeRequest) -> Result<Self, Error> {
        match request.topic.as_str() {
            "queries" => {
                let (deployment, caller) = service.scope(&principal, &request.deployment, request.caller.as_ref())?;
                let group = queries::subscribe(service.app.backend(), &deployment, &caller, &request.arguments);
                let (keys, group) = group.await?;
                let context = Context {
                    id: service.streams.id(request, &principal.credential),
                    nudges: service.app.nudges().follow(&principal.credential),
                    grant: Grant::new(
                        service.credentials.clone(),
                        principal,
                        &request.deployment,
                        request.caller.as_ref(),
                    ),
                    epoch: service.epoch,
                };
                Ok(Self::Queries(queries::Queries::new(keys, group, context)))
            }
            topic if topic.starts_with("command/") => {
                command::open(service, principal, request).await.map(Self::Command)
            }
            topic if topic.starts_with("gateway/") => gateway::open(service, principal, request).map(Self::Gateway),
            topic if topic.starts_with("jvm/") => jvm::open(service, principal, request).map(Self::Jvm),
            "nodes" | "players" => operator::open(service, &principal, request).map(Self::Operator),
            _ => Err(errors::invalid("unknown topic")),
        }
    }

    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        match self {
            Self::Queries(queries) => queries.run(sender, stop).await,
            Self::Command(command) => command.run(sender, stop).await,
            Self::Gateway(gateway) => gateway.run(sender, stop).await,
            Self::Jvm(jvm) => jvm.run(sender, stop).await,
            Self::Operator(operator) => operator.run(sender, stop).await,
        }
    }
}

/// Sends `update` if the grant still holds, and otherwise ends the stream with why it lapsed. Returns whether the
/// stream continues.
fn send(grant: &Grant, sender: &Sender, update: Update) -> bool {
    if let Err(error) = grant.check() {
        sender.fail(error);
        return false;
    }
    sender.send(update);
    true
}

/// Waits for `receiver` to change, or forever once its sender is gone.
async fn changed<T>(receiver: &mut watch::Receiver<T>) {
    if receiver.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}
