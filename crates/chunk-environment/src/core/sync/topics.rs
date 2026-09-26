//! Subscription topics, by name.

mod queries;

use super::{SyncService, auth::Principal, errors, streams::Sender};
use chunk_proto::sync::v1::{Error, SubscribeRequest};
use tokio_util::sync::CancellationToken;

pub(super) enum Topic {
    Queries(queries::Queries),
}

impl Topic {
    /// Checks and subscribes to the topic `request` names.
    pub async fn open(service: &SyncService, principal: &Principal, request: &SubscribeRequest) -> Result<Self, Error> {
        match request.topic.as_str() {
            "queries" => {
                let (deployment, caller) = service.scope(principal, &request.deployment, request.caller.as_ref())?;
                let stream = service.streams.id(request, &principal.credential);
                let backend = service.app.backend();
                let queries =
                    queries::Queries::open(backend, &deployment, &caller, &request.arguments, stream, service.epoch);
                Ok(Self::Queries(queries.await?))
            }
            _ => Err(errors::invalid("unknown topic")),
        }
    }

    pub async fn run(self, sender: Sender, stop: CancellationToken) {
        match self {
            Self::Queries(queries) => queries.run(sender, stop).await,
        }
    }
}
