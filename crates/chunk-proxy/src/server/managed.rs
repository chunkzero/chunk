mod relay;

use std::{io, time::Duration};

use chunk_proto::v1::{ActivateClaim, Assignment, ClaimIdentity, ClaimPhase, ClaimRequest};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep, timeout},
};

use super::{
    authentication::Authenticated,
    configuration, gameplay,
    platform::{Platform, request},
    transport::{Transport, invalid_data},
};

const WAIT_TIMEOUT: Duration = Duration::from_secs(45);

struct ClaimGuard {
    platform: Platform,
    claim: ClaimRequest,
    armed: bool,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let platform = self.platform.clone();
        let claim = self.claim.clone();
        self.platform.cleanup.spawn(async move {
            for attempt in 0..2 {
                let Ok(message) = request(claim.clone(), &platform.target.control.token) else {
                    return;
                };
                match platform.control.clone().cancel(message).await {
                    Err(error)
                        if attempt == 0
                            && error.code() == tonic::Code::FailedPrecondition
                            && error.message() == "unknown claim" =>
                    {
                        sleep(Duration::from_millis(100)).await;
                    }
                    _ => return,
                }
            }
        });
    }
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    authenticated: Authenticated<S>,
    platform: &Platform,
    deadline: Duration,
) -> io::Result<()> {
    let claim = login_claim(&authenticated.profile, platform);
    let destination = async {
        let mut claim = claim;
        let identity = claim.identity.as_ref().ok_or_else(|| invalid_data("missing authenticated identity"))?;
        claim.demand = Some(platform.route(&identity.uuid, &identity.username).await?);
        // Construct before sending: cancellation must cover a claim whose reply was lost.
        let guard = ClaimGuard { platform: platform.clone(), claim, armed: true };
        let mut message = request(guard.claim.clone(), &platform.target.control.token)?;
        message.set_timeout(WAIT_TIMEOUT);
        let assignment = platform.control.clone().claim(message).await.map_err(claim_error)?.into_inner();
        validate(&assignment, &guard)?;
        Ok((guard, assignment))
    };
    let (mut authenticated, mut settings, (mut guard, mut assignment)) =
        configuration::wait_for_destination(authenticated, destination, deadline.min(WAIT_TIMEOUT)).await?;
    loop {
        let mut internal = timeout(deadline.min(WAIT_TIMEOUT), open(&assignment, &guard, &authenticated, &settings))
            .await
            .map_err(io::Error::other)??;
        timeout(
            deadline.min(WAIT_TIMEOUT),
            Box::pin(configuration::relay(&mut authenticated.transport, &mut internal, &mut settings)),
        )
        .await
        .map_err(io::Error::other)??;
        let identity = assignment.claim.clone().ok_or_else(|| invalid_data("missing claim identity"))?;
        let arrival = relay::until(
            &mut authenticated.transport,
            &mut internal,
            &mut settings,
            Box::pin(arrive(&guard, identity.clone())),
            true,
        )
        .await?;
        if let Err(error) = arrival {
            let _ =
                configuration::disconnect(&mut authenticated.transport, 0x20, "Server temporarily unavailable").await;
            return Err(error);
        }
        tracing::info!(operation = %guard.claim.operation_id, player = %guard.claim.identity.as_ref().map_or("", |identity| identity.uuid.as_str()), "player arrived in managed session");
        let next = relay::until(
            &mut authenticated.transport,
            &mut internal,
            &mut settings,
            Box::pin(next_move(&guard, &identity, authenticated.protocol_version)),
            true,
        )
        .await??;
        timeout(
            Duration::from_secs(10),
            relay::start_configuration(&mut authenticated.transport, &mut internal, &mut settings),
        )
        .await
        .map_err(io::Error::other)??;
        // The client's acknowledgment fences all remaining source PLAY input.
        if let Err(error) = withdraw(&guard, &identity).await {
            let _ = configuration::disconnect(&mut authenticated.transport, 0x02, "Session move unavailable").await;
            return Err(error);
        }
        guard.armed = false;
        drop(internal);
        (guard, assignment) = next;
    }
}

fn login_claim(profile: &chunk_protocol::versions::v26_1::LoginSuccess, platform: &Platform) -> ClaimRequest {
    ClaimRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        proxy_id: platform.proxy_id.clone(),
        connection_id: uuid::Uuid::new_v4().to_string(),
        identity: Some(gameplay::identity(profile)),
        demand: None,
        source: None,
    }
}

async fn open<S>(
    assignment: &Assignment,
    guard: &ClaimGuard,
    authenticated: &Authenticated<S>,
    settings: &chunk_protocol::versions::v26_1::ConfigurationClientInformation,
) -> io::Result<Transport<tokio::net::TcpStream>> {
    validate(assignment, guard)?;
    if assignment.configuration.as_ref().is_none_or(|c| c.protocol != authenticated.protocol_version) {
        return Err(invalid_data("destination protocol differs from client"));
    }
    guard
        .platform
        .control
        .clone()
        .activate(request(ActivateClaim { claim: assignment.claim.clone() }, &guard.platform.target.control.token)?)
        .await
        .map_err(io::Error::other)?;
    let preparation = assignment.preparation.clone().ok_or_else(|| invalid_data("missing preparation"))?;
    gameplay::login(authenticated, settings, preparation).await
}

async fn next_move(
    source: &ClaimGuard,
    identity: &ClaimIdentity,
    protocol: i32,
) -> io::Result<(ClaimGuard, Assignment)> {
    loop {
        sleep(Duration::from_millis(500)).await;
        let polled = source
            .platform
            .control
            .clone()
            .poll_move(request(source.claim.clone(), &source.platform.target.control.token)?)
            .await;
        let Ok(response) = polled else {
            continue;
        };
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
        let guard = ClaimGuard { platform: source.platform.clone(), claim, armed: true };
        let prepare = async {
            guard.platform.approve_move(&guard.claim).await?;
            let mut message = request(guard.claim.clone(), &guard.platform.target.control.token)?;
            message.set_timeout(WAIT_TIMEOUT);
            let assignment =
                guard.platform.control.clone().claim(message).await.map_err(io::Error::other)?.into_inner();
            validate(&assignment, &guard)?;
            if assignment.configuration.as_ref().is_none_or(|c| c.protocol != protocol) {
                return Err(invalid_data("destination protocol differs from client"));
            }
            Ok::<_, io::Error>(assignment)
        };
        match timeout(WAIT_TIMEOUT, prepare).await {
            Ok(Ok(assignment)) => return Ok((guard, assignment)),
            Ok(Err(error)) => tracing::warn!(%error, "move preparation failed; source remains active"),
            Err(_) => tracing::warn!("move preparation timed out; source remains active"),
        }
    }
}

async fn withdraw(source: &ClaimGuard, identity: &ClaimIdentity) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        let result = source
            .platform
            .control
            .clone()
            .cancel(request(source.claim.clone(), &source.platform.target.control.token)?)
            .await;
        match result {
            Ok(response) if response.get_ref() == identity => return Ok(()),
            Ok(_) => return Err(invalid_data("withdrawal identity mismatch")),
            Err(error)
                if matches!(
                    error.code(),
                    tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Unknown
                ) => {}
            Err(error) => return Err(io::Error::other(error)),
        }
        // Repeating cancellation uses the same durable operation and exact source generation.
        sleep(Duration::from_millis(100)).await;
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "source withdrawal unresolved"))
}

fn claim_error(error: tonic::Status) -> io::Error {
    if error.code() == tonic::Code::FailedPrecondition && error.message() == "player already owned" {
        io::Error::new(io::ErrorKind::PermissionDenied, "You are already connected.")
    } else {
        io::Error::other(error)
    }
}

fn validate(assignment: &Assignment, guard: &ClaimGuard) -> io::Result<()> {
    let claim = assignment.claim.as_ref().ok_or_else(|| invalid_data("missing claim"))?;
    let delivery = assignment.delivery.as_ref().ok_or_else(|| invalid_data("missing delivery"))?;
    let config = assignment.configuration.as_ref().ok_or_else(|| invalid_data("missing configuration"))?;
    let backend = &guard.platform.target.backend;
    let deployment = chunk_proto::v1::DeploymentRef {
        environment: backend.environment.clone(),
        deployment: backend.deployment.clone(),
    };
    if assignment.phase != i32::from(ClaimPhase::Reserved)
        || claim.operation_id != guard.claim.operation_id
        || claim.proxy_id != guard.claim.proxy_id
        || claim.membership_generation == 0
        || claim.delivery_generation == 0
        || delivery.operation_id != claim.operation_id
        || delivery.proxy_id != claim.proxy_id
        || delivery.connection_id != guard.claim.connection_id
        || delivery.identity.as_ref().is_some_and(|identity| Some(identity) != guard.claim.identity.as_ref())
        || delivery.membership_generation != claim.membership_generation
        || delivery.owner_generation != claim.delivery_generation
        || delivery.session_generation == 0
        || delivery.session.as_ref().is_none_or(|session| session.id.is_empty())
        || delivery.player.as_ref().map(|player| &player.id)
            != guard.claim.identity.as_ref().map(|identity| &identity.uuid)
        || config.deployment.as_ref() != Some(&deployment)
        || delivery.deployment != config.deployment
        || delivery.process_generation != config.process_generation
        || delivery.runtime_id != config.runtime_id
        || delivery.protocol != config.protocol
        || config.runtime_id.is_empty()
        || config.process_generation == 0
    {
        return Err(invalid_data("control assignment identity mismatch"));
    }
    let prepared = assignment.preparation.as_ref().ok_or_else(|| invalid_data("missing preparation"))?;
    let address: std::net::SocketAddr = prepared.endpoint.parse().map_err(invalid_data)?;
    if prepared.operation_id != claim.operation_id
        || prepared.capability.len() != 32
        || !address.ip().is_loopback()
        || address.port() == 0
    {
        return Err(invalid_data("invalid player preparation"));
    }
    Ok(())
}

async fn arrive(guard: &ClaimGuard, identity: ClaimIdentity) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let activation = ActivateClaim { claim: Some(identity.clone()) };
    let mut activated = true;
    while Instant::now() < deadline {
        let mut client = guard.platform.control.clone();
        let result = if activated {
            client.inspect(request(guard.claim.clone(), &guard.platform.target.control.token)?).await
        } else {
            client.activate(request(activation.clone(), &guard.platform.target.control.token)?).await
        };
        match result {
            Ok(response) => {
                let assignment = response.into_inner();
                if assignment.claim.as_ref() != Some(&identity) {
                    return Err(invalid_data("activation claim mismatch"));
                }
                match ClaimPhase::try_from(assignment.phase).map_err(invalid_data)? {
                    ClaimPhase::Arrived => return Ok(()),
                    ClaimPhase::Activating | ClaimPhase::Attached => activated = true,
                    ClaimPhase::Reserved => {}
                    _ => return Err(io::Error::other("claim withdrawn during activation")),
                }
            }
            Err(error)
                if matches!(
                    error.code(),
                    tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Unknown
                ) => {}
            Err(error) => return Err(io::Error::other(error)),
        }
        sleep(Duration::from_millis(500)).await;
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "session arrival timed out"))
}
