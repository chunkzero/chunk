//! Waking a sleeping environment through management's `Wake`, then waiting for its route to list gateways.

use std::net::{IpAddr, SocketAddr};

use chunk_management::{
    Client,
    v1::{Route, WakeOutcome, WakeReason, WakeRequest},
};
use tokio::time::{Instant, timeout_at};

use crate::routes::Routes;

/// Why an environment isn't ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The client recently failed authentication, so it may not wake the environment.
    Blocked,
    /// The environment's wake limit is reached.
    Throttled,
    /// The deadline passed first.
    TimedOut,
    /// Management refused or failed the wake, or the route was removed.
    Failed,
}

impl Refusal {
    /// What a player who logged in is shown.
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::Blocked => "This server is sleeping. Try again later.",
            Self::Throttled | Self::TimedOut => "This server is starting. Try again in a moment.",
            Self::Failed => "This server is unavailable. Try again later.",
        }
    }
}

/// Asks management to wake `route`'s environment for `client`, then waits until `hostname`'s route lists gateways, all
/// before `deadline`.
pub(crate) async fn wake(
    management: &Client,
    routes: &Routes,
    route: &Route,
    reason: WakeReason,
    client: IpAddr,
    deadline: Instant,
) -> Result<Vec<SocketAddr>, Refusal> {
    let request = WakeRequest {
        environment_id: route.environment_id.clone(),
        reason: reason.into(),
        client_address: client.to_canonical().to_string(),
    };
    let outcome = match timeout_at(deadline, management.wake(&request)).await {
        Ok(Ok(response)) => response.outcome(),
        Ok(Err(error)) => {
            tracing::warn!(environment = route.environment_id, %error, "wake failed");
            return Err(Refusal::Failed);
        }
        Err(_) => return Err(Refusal::TimedOut),
    };
    match outcome {
        WakeOutcome::Waking | WakeOutcome::Awake => {}
        WakeOutcome::Blocked => return Err(Refusal::Blocked),
        WakeOutcome::Throttled => return Err(Refusal::Throttled),
        WakeOutcome::Unspecified => return Err(Refusal::Failed),
    }
    match timeout_at(deadline, routes.ready(&route.hostname)).await {
        Ok(Some(gateways)) => Ok(gateways),
        Ok(None) => Err(Refusal::Failed),
        Err(_) => Err(Refusal::TimedOut),
    }
}
