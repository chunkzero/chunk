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

/// Asks management to wake `route`'s environment for `client`, then waits until its route lists gateways, all before
/// `deadline`. Gateways that appear while `Wake` is in flight are used at once, and a refusal, error or timeout is
/// overridden by gateways the route lists by then, since another client may have woken the environment meanwhile.
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
    let ready = timeout_at(deadline, routes.ready(route));
    tokio::pin!(ready);
    let waited = |ready: Result<Option<_>, _>| match ready {
        Ok(Some(gateways)) => Ok(gateways),
        Ok(None) => Err(Refusal::Failed),
        Err(_) => Err(Refusal::TimedOut),
    };
    let woken = tokio::select! {
        ready = &mut ready => waited(ready),
        woken = timeout_at(deadline, management.wake(&request)) => match woken {
            Ok(Ok(response)) => match response.outcome() {
                WakeOutcome::Waking | WakeOutcome::Awake => waited(ready.await),
                WakeOutcome::Blocked => Err(Refusal::Blocked),
                WakeOutcome::Throttled => Err(Refusal::Throttled),
                WakeOutcome::Unspecified => Err(Refusal::Failed),
            },
            Ok(Err(error)) => {
                tracing::warn!(environment = route.environment_id, %error, "wake failed");
                Err(Refusal::Failed)
            }
            Err(_) => Err(Refusal::TimedOut),
        },
    };
    woken.or_else(|refusal| routes.gateways(route).filter(|gateways| !gateways.is_empty()).ok_or(refusal))
}
