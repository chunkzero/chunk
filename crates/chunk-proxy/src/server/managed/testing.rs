//! A managed connection's claim lifecycle without a Minecraft client, for tests against a real core.

use std::io;

use chunk_proto::v1::{ClaimRequest, Identity, SessionDemand};

use super::{Assignment, ClaimGuard, activate, arrive, claim};
use crate::{PlatformTarget, Retarget, server::platform::Platform};

/// Claims a login of player `uuid` into `demand` through `target` as a managed connection does, activates it, waits
/// until the gateway's topic shows it arrived, then withdraws it. Returns the session the player joined.
/// # Errors
/// Reports a refused or failed claim, and an arrival the claim view never shows.
pub async fn login(target: PlatformTarget, uuid: &str, username: &str, demand: SessionDemand) -> io::Result<String> {
    let platform = Platform::new(target)?;
    let (guard, assignment) = arrived(&platform, uuid, username, demand).await?;
    drop(guard);
    platform.cleanup.close();
    platform.cleanup.wait().await;
    Ok(assignment.session)
}

/// Claims a login through `proxy`'s platform as [`login`] does, but leaves its claim open, as a player still connected
/// through the proxy would. Returns the claim's operation ID.
/// # Errors
/// Reports a refused or failed claim, and an arrival the claim view never shows.
pub async fn hold(proxy: &Retarget, uuid: &str, username: &str, demand: SessionDemand) -> io::Result<String> {
    let (mut guard, _) = arrived(&proxy.platform(), uuid, username, demand).await?;
    guard.armed = false;
    Ok(guard.claim.operation_id.clone())
}

async fn arrived(
    platform: &Platform,
    uuid: &str,
    username: &str,
    demand: SessionDemand,
) -> io::Result<(ClaimGuard, Assignment)> {
    let login = ClaimRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        proxy_id: platform.proxy_id.clone(),
        connection_id: uuid::Uuid::new_v4().to_string(),
        identity: Some(Identity { uuid: uuid.into(), username: username.into(), properties: vec![] }),
        demand: Some(demand),
        ..ClaimRequest::default()
    };
    let guard = ClaimGuard { platform: platform.clone(), claim: login, armed: true, failure: None };
    let assignment = claim(&guard).await?.ok_or_else(|| io::Error::other("core asked to route the login again"))?;
    activate(&guard).await?;
    arrive(&guard, assignment.identity.clone()).await?;
    Ok((guard, assignment))
}
