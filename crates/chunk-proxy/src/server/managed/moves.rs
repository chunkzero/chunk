use super::{Assignment, Claim, ClaimGuard, ClaimIdentity, Platform, Retarget, WAIT_TIMEOUT, claim, invalid_data};
use crate::server::platform::{RPC_TIMEOUT, failure};
use chunk_proto::sync::v1::{Position, error::Code};
use std::{io, time::Duration};
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};

const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Waits for control to queue a move from the arrived `identity`, then prepares its destination, which a move places in
/// the release `current` names.
pub(super) async fn next_move(
    source: &ClaimGuard,
    identity: &ClaimIdentity,
    protocol: i32,
    current: &Retarget,
) -> io::Result<(ClaimGuard, Assignment)> {
    let mut abandoned: Option<ClaimGuard> = None;
    // A view past an abandonment no longer shows its move, and failures are final.
    let mut settled: Option<Position> = None;
    loop {
        if let Some(mut guard) = abandoned.take() {
            match guard.cancel().await {
                Ok((_, position)) => {
                    guard.armed = false;
                    settled = position;
                }
                Err(error) => {
                    tracing::debug!(%error, operation = %guard.claim.operation_id, "move abandonment unresolved");
                    abandoned = Some(guard);
                    sleep(RETRY_INTERVAL).await;
                    continue;
                }
            }
        }
        let pending = source.platform.claims(|view| {
            if !view.passed(settled.as_ref()) {
                return None;
            }
            view.claim(identity)?.pending_move.clone()
        });
        let pending = pending.await?;
        let demand = pending.destination.ok_or_else(|| invalid_data("move without destination"))?;
        let platform = current.platform();
        let claim = Claim {
            operation_id: pending.operation_id,
            demand,
            source: Some(identity.clone()),
            deployment: platform.target.deployment.clone(),
            ..source.claim.clone()
        };
        let mut guard = ClaimGuard { platform, claim, armed: true, failure: None };
        match prepare(source, &mut guard, protocol).await {
            Ok(Some(assignment)) => return Ok((guard, assignment)),
            // The deployment that approved the move is no longer current, so a newer one approves it again.
            Ok(None) => {
                guard.armed = false;
                sleep(RETRY_INTERVAL).await;
            }
            Err(error) => {
                let reason = failure_reason(&error);
                let reason = if reason.is_empty() { "move preparation failed".into() } else { reason };
                tracing::warn!(%reason, operation = %guard.claim.operation_id, "move abandoned; source remains active");
                guard.failure = Some(reason.chars().take(1024).collect());
                abandoned = Some(guard);
            }
        }
    }
}

/// Prepares `destination` under its deployment's approval, or returns `None` when core no longer places moves there.
/// A move core already reserved in another deployment is approved there too, and `destination` is bound to it.
async fn prepare(source: &ClaimGuard, destination: &mut ClaimGuard, protocol: i32) -> io::Result<Option<Assignment>> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let mut last_error = None;
    loop {
        let error = match timeout_at(deadline, attempt(source, &mut *destination, protocol)).await {
            Ok(Ok(assignment)) => return Ok(assignment),
            Ok(Err(error)) => error,
            Err(_) => {
                return Err(preparation_timeout(last_error.as_ref()));
            }
        };
        if Instant::now() >= deadline {
            return Err(preparation_timeout(last_error.as_ref().or(Some(&error))));
        }
        if !transient(&error) {
            return Err(error);
        }
        last_error = Some(error);
        sleep_until((Instant::now() + RETRY_INTERVAL).min(deadline)).await;
        if Instant::now() >= deadline {
            return Err(preparation_timeout(last_error.as_ref()));
        }
    }
}

fn failure_reason(error: &io::Error) -> String {
    rpc_error(error).map_or_else(|| error.to_string(), |status| status.message().into())
}

fn preparation_timeout(last_error: Option<&io::Error>) -> io::Error {
    let mut reason = format!("move preparation timed out after {} seconds", WAIT_TIMEOUT.as_secs());
    if let Some(error) = last_error {
        reason.push_str(": ");
        reason.push_str(&failure_reason(error));
    }
    io::Error::new(io::ErrorKind::TimedOut, reason)
}

async fn attempt(source: &ClaimGuard, destination: &mut ClaimGuard, protocol: i32) -> io::Result<Option<Assignment>> {
    let platform = destination.platform.clone();
    check_move(&platform, &destination.claim).await?;
    platform.approve_move(&source.platform, &source.claim, &destination.claim).await?;
    check_move(&platform, &destination.claim).await?;
    let Some(assignment) = claim(destination).await? else { return Ok(None) };
    if !assignment.deployment.is_empty() && assignment.deployment != destination.claim.deployment {
        let placed = platform.bind(&assignment.deployment);
        let claim = Claim {
            deployment: assignment.deployment.clone(),
            demand: assignment.destination.clone(),
            ..destination.claim.clone()
        };
        placed.approve_move(&source.platform, &source.claim, &claim).await?;
        (destination.platform, destination.claim) = (placed, claim);
    }
    if assignment.protocol != protocol {
        return Err(invalid_data("destination protocol differs from client"));
    }
    check_move(&destination.platform, &destination.claim).await?;
    Ok(Some(assignment))
}

/// Whether `error` may pass on a retry under the same operation ID.
pub(super) fn transient(error: &io::Error) -> bool {
    rpc_error(error)
        .is_some_and(|status| matches!(status.code(), tonic::Code::Unavailable | tonic::Code::DeadlineExceeded))
        || failure(error).is_some_and(|failure| matches!(failure.code(), Code::Unavailable | Code::Overloaded))
        || error.kind() == io::ErrorKind::TimedOut
        || matches!(error.get_ref(), Some(cause) if cause.is::<tokio::time::error::Elapsed>())
}

pub(super) fn rpc_error(error: &io::Error) -> Option<&tonic::Status> {
    error.get_ref()?.downcast_ref()
}

/// Confirms from the claim view that `destination` is still the move pending from its source.
pub(super) async fn check_move(platform: &Platform, destination: &Claim) -> io::Result<()> {
    let source = destination.source.as_ref().ok_or_else(|| invalid_data("move without source"))?;
    let pending = platform.claims(|view| {
        let pending = view.claim(source)?.pending_move.as_ref();
        Some(pending.is_some_and(|pending| pending.operation_id == destination.operation_id))
    });
    match timeout(RPC_TIMEOUT, pending).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(invalid_data("move canceled or source ownership changed")),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "claim view unavailable")),
    }
}
