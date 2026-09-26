use super::{Assignment, ClaimGuard, ClaimIdentity, ClaimRequest, Platform, WAIT_TIMEOUT, invalid_data, validate};
use crate::server::platform::RPC_TIMEOUT;
use std::{io, time::Duration};
use tokio::time::{Instant, sleep, sleep_until, timeout, timeout_at};

const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Waits for control to queue a move from the arrived `identity`, then prepares its destination.
pub(super) async fn next_move(
    source: &ClaimGuard,
    identity: &ClaimIdentity,
    protocol: i32,
) -> io::Result<(ClaimGuard, Assignment)> {
    let mut abandoned: Option<ClaimGuard> = None;
    let mut failed: Option<String> = None;
    loop {
        if let Some(mut guard) = abandoned.take() {
            match guard.cancel().await {
                Ok(()) => guard.armed = false,
                Err(error) => {
                    tracing::debug!(%error, operation = %guard.claim.operation_id, "move abandonment unresolved");
                    abandoned = Some(guard);
                    sleep(RETRY_INTERVAL).await;
                    continue;
                }
            }
        }
        // The view may still show a move this connection abandoned; failures are final, so skip it.
        let claim = source
            .platform
            .claims(|view| {
                view.claim(identity)?.pending_move.clone().filter(|claim| Some(&claim.operation_id) != failed.as_ref())
            })
            .await?;
        if claim.source.as_ref() != Some(identity)
            || claim.proxy_id != source.claim.proxy_id
            || claim.connection_id != source.claim.connection_id
            || claim.identity != source.claim.identity
        {
            return Err(invalid_data("move identity mismatch"));
        }
        let mut guard = ClaimGuard { platform: source.platform.clone(), claim, armed: true, failure: None };
        match prepare(source, &guard, protocol).await {
            Ok(assignment) => return Ok((guard, assignment)),
            Err(error) => {
                let reason = failure_reason(&error);
                let reason = if reason.is_empty() { "move preparation failed".into() } else { reason };
                tracing::warn!(%reason, operation = %guard.claim.operation_id, "move abandoned; source remains active");
                guard.failure = Some(reason.chars().take(1024).collect());
                failed = Some(guard.claim.operation_id.clone());
                abandoned = Some(guard);
            }
        }
    }
}

async fn prepare(source: &ClaimGuard, destination: &ClaimGuard, protocol: i32) -> io::Result<Assignment> {
    let deadline = Instant::now() + WAIT_TIMEOUT;
    let mut last_error = None;
    loop {
        let error = match timeout_at(deadline, attempt(source, destination, protocol)).await {
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

async fn attempt(source: &ClaimGuard, destination: &ClaimGuard, protocol: i32) -> io::Result<Assignment> {
    let platform = &destination.platform;
    check_move(platform, &destination.claim).await?;
    platform.approve_move(&source.claim, &destination.claim).await?;
    check_move(platform, &destination.claim).await?;
    let mut message = destination.platform.control_request(destination.claim.clone())?;
    message.set_timeout(WAIT_TIMEOUT);
    let assignment = destination.platform.control.clone().claim(message).await.map_err(io::Error::other)?.into_inner();
    validate(&assignment, destination)?;
    if assignment.configuration.as_ref().is_none_or(|c| c.protocol != protocol) {
        return Err(invalid_data("destination protocol differs from client"));
    }
    check_move(platform, &destination.claim).await?;
    Ok(assignment)
}

fn transient(error: &io::Error) -> bool {
    rpc_error(error)
        .is_some_and(|status| matches!(status.code(), tonic::Code::Unavailable | tonic::Code::DeadlineExceeded))
        || error.kind() == io::ErrorKind::TimedOut
        || matches!(error.get_ref(), Some(cause) if cause.is::<tokio::time::error::Elapsed>())
}

pub(super) fn rpc_error(error: &io::Error) -> Option<&tonic::Status> {
    error.get_ref()?.downcast_ref()
}

/// Confirms from the claim view that `destination` is still the move pending from its source.
pub(super) async fn check_move(platform: &Platform, destination: &ClaimRequest) -> io::Result<()> {
    let source = destination.source.as_ref().ok_or_else(|| invalid_data("move without source"))?;
    let pending = platform.claims(|view| Some(view.claim(source)?.pending_move.as_ref() == Some(destination)));
    match timeout(RPC_TIMEOUT, pending).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(invalid_data("move canceled or source ownership changed")),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "claim view unavailable")),
    }
}
