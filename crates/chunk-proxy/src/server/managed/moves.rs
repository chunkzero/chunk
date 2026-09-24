use super::{Assignment, ClaimGuard, ClaimIdentity, ClaimRequest, WAIT_TIMEOUT, invalid_data, validate};
use std::{io, time::Duration};
use tokio::time::{Instant, sleep, sleep_until, timeout_at};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

pub(super) async fn next_move(
    source: &ClaimGuard,
    identity: &ClaimIdentity,
    protocol: i32,
) -> io::Result<(ClaimGuard, Assignment)> {
    let mut abandoned: Option<ClaimGuard> = None;
    loop {
        sleep(POLL_INTERVAL).await;
        let polled =
            source.platform.control.clone().poll_move(source.platform.control_request(source.claim.clone())?).await;
        let Ok(response) = polled else {
            continue;
        };
        // Recover a lost failure report when control is reachable, without preparing the move again.
        if let Some(mut guard) = abandoned.take() {
            match guard.cancel().await {
                Ok(()) => guard.armed = false,
                Err(error) => {
                    tracing::debug!(%error, operation = %guard.claim.operation_id, "move abandonment unresolved");
                    abandoned = Some(guard);
                }
            }
            continue;
        }
        let Some(claim) = response.into_inner().claim else {
            continue;
        };
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
        sleep_until((Instant::now() + POLL_INTERVAL).min(deadline)).await;
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
    check_move(source, &destination.claim).await?;
    destination.platform.approve_move(&source.claim, &destination.claim).await?;
    check_move(source, &destination.claim).await?;
    let mut message = destination.platform.control_request(destination.claim.clone())?;
    message.set_timeout(WAIT_TIMEOUT);
    let assignment = destination.platform.control.clone().claim(message).await.map_err(io::Error::other)?.into_inner();
    validate(&assignment, destination)?;
    if assignment.configuration.as_ref().is_none_or(|c| c.protocol != protocol) {
        return Err(invalid_data("destination protocol differs from client"));
    }
    check_move(source, &destination.claim).await?;
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

pub(super) async fn check_move(source: &ClaimGuard, destination: &ClaimRequest) -> io::Result<()> {
    let pending = source
        .platform
        .control
        .clone()
        .poll_move(source.platform.control_request(source.claim.clone())?)
        .await
        .map_err(io::Error::other)?
        .into_inner();
    if pending.claim.as_ref() != Some(destination) {
        return Err(invalid_data("move canceled or source ownership changed"));
    }
    Ok(())
}
