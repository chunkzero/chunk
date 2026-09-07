use std::{io, time::Duration};

use chunk_proto::v1::{ActivateClaim, Assignment, ClaimIdentity, ClaimPhase, ClaimRequest};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep, timeout},
};

use super::{
    authentication::Authenticated,
    configuration, gameplay,
    platform::{Platform, RPC_TIMEOUT, request},
    transport::invalid_data,
};

const WAIT_TIMEOUT: Duration = Duration::from_secs(45);
const INPUT_LIMIT: usize = 65_536;

struct ClaimGuard {
    platform: Platform,
    claim: ClaimRequest,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        let platform = self.platform.clone();
        let claim = self.claim.clone();
        tokio::spawn(async move {
            if let Ok(request) = request(claim, &platform.target.control.token) {
                let _ = platform.control.clone().cancel(request).await;
            }
        });
    }
}

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    authenticated: Authenticated<S>,
    platform: &Platform,
    deadline: Duration,
) -> io::Result<()> {
    let profile = &authenticated.profile;
    let claim = ClaimRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        proxy_id: platform.proxy_id.clone(),
        connection_id: uuid::Uuid::new_v4().to_string(),
        identity: Some(gameplay::identity(profile)),
        demand: None,
    };
    let destination = async {
        let mut claim = claim;
        let identity = claim
            .identity
            .as_ref()
            .ok_or_else(|| invalid_data("missing authenticated identity"))?;
        claim.demand = Some(platform.route(&identity.uuid, &identity.username).await?);
        // Construct before sending: cancellation must cover a claim whose reply was lost.
        let guard = ClaimGuard {
            platform: platform.clone(),
            claim,
        };
        let mut message = request(guard.claim.clone(), &platform.target.control.token)?;
        message.set_timeout(WAIT_TIMEOUT);
        let assignment = platform
            .control
            .clone()
            .claim(message)
            .await
            .map_err(io::Error::other)?
            .into_inner();
        validate(&assignment, &guard)?;
        Ok((guard, assignment))
    };
    let (mut authenticated, settings, (guard, assignment)) =
        configuration::wait_for_destination(authenticated, destination, deadline.min(WAIT_TIMEOUT)).await?;
    let config = assignment
        .configuration
        .as_ref()
        .ok_or_else(|| invalid_data("missing configuration"))?;
    if config.protocol != authenticated.protocol_version {
        return Err(invalid_data("destination protocol differs from client"));
    }
    let preparation = assignment
        .preparation
        .as_ref()
        .ok_or_else(|| invalid_data("missing preparation"))?;
    if preparation.operation_id != guard.claim.operation_id {
        return Err(invalid_data("preparation operation mismatch"));
    }
    let claim_identity = assignment
        .claim
        .clone()
        .ok_or_else(|| invalid_data("missing claim identity"))?;
    guard
        .platform
        .control
        .clone()
        .activate(request(
            ActivateClaim {
                claim: Some(claim_identity.clone()),
            },
            &guard.platform.target.control.token,
        )?)
        .await
        .map_err(io::Error::other)?;
    let mut internal = timeout(
        deadline.min(WAIT_TIMEOUT),
        gameplay::login(&authenticated, &settings, preparation.clone()),
    )
    .await
    .map_err(io::Error::other)??;
    timeout(
        deadline.min(WAIT_TIMEOUT),
        Box::pin(configuration::relay(&mut authenticated.transport, &mut internal)),
    )
    .await
    .map_err(io::Error::other)??;
    let arrival = arrive(&guard, claim_identity);
    tokio::pin!(arrival);
    let mut arrived = false;
    loop {
        tokio::select! {
            result = &mut arrival, if !arrived => {
                if let Err(error) = result {
                    let _ = configuration::disconnect(&mut authenticated.transport, 0x20, "Server temporarily unavailable").await;
                    return Err(error);
                }
                arrived = true;
                tracing::info!("player arrived in managed session");
            }
            frame = authenticated.transport.read_frame(INPUT_LIMIT) => {
                timeout(RPC_TIMEOUT, internal.write_body(&frame?)).await.map_err(io::Error::other)??;
            }
            frame = internal.read_frame(chunk_protocol::MAX_FRAME_SIZE) => {
                timeout(RPC_TIMEOUT, authenticated.transport.write_body(&frame?)).await.map_err(io::Error::other)??;
            }
        }
    }
}

fn validate(assignment: &Assignment, guard: &ClaimGuard) -> io::Result<()> {
    let claim = assignment.claim.as_ref().ok_or_else(|| invalid_data("missing claim"))?;
    let delivery = assignment
        .delivery
        .as_ref()
        .ok_or_else(|| invalid_data("missing delivery"))?;
    let config = assignment
        .configuration
        .as_ref()
        .ok_or_else(|| invalid_data("missing configuration"))?;
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
        || delivery
            .identity
            .as_ref()
            .is_some_and(|identity| Some(identity) != guard.claim.identity.as_ref())
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
    Ok(())
}

async fn arrive(guard: &ClaimGuard, identity: ClaimIdentity) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let activation = ActivateClaim {
        claim: Some(identity.clone()),
    };
    let mut activated = false;
    while Instant::now() < deadline {
        let mut client = guard.platform.control.clone();
        let result = if activated {
            client
                .inspect(request(guard.claim.clone(), &guard.platform.target.control.token)?)
                .await
        } else {
            client
                .activate(request(activation.clone(), &guard.platform.target.control.token)?)
                .await
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
        sleep(Duration::from_millis(100)).await;
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "session arrival timed out"))
}
