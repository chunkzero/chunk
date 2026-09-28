mod commands;
mod moves;
mod relay;

#[cfg(feature = "bench-support")]
pub mod benchmark;
#[cfg(feature = "test-support")]
pub mod testing;
#[cfg(test)]
mod tests;

use std::{io, time::Duration};

use chunk_proto::{
    sync::v1::{
        AbandonMoveArguments, ActivateResult, ClaimArguments, ClaimAssignment, ClaimPhase, ClaimRefusal, ClaimResult,
        GatewayLogin, PlayerIdentity, PlayerProperty, Position, SessionDemand, WithdrawResult, claim_result::Outcome,
    },
    v1::{ClaimIdentity, ClaimRequest, PlayerPreparation},
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::oneshot,
    time::{Instant, sleep, timeout},
};

use super::{
    Retarget,
    authentication::Authenticated,
    configuration, gameplay,
    platform::{Lifecycle, Platform, RPC_TIMEOUT, generation},
    transport::{Transport, invalid_data},
};

use moves::{check_move, next_move};

const WAIT_TIMEOUT: Duration = Duration::from_secs(45);

struct ClaimGuard {
    platform: Platform,
    claim: ClaimRequest,
    armed: bool,
    failure: Option<String>,
}

impl Drop for ClaimGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let guard = Self {
            platform: self.platform.clone(),
            claim: self.claim.clone(),
            armed: false,
            failure: self.failure.clone(),
        };
        self.platform.cleanup.spawn(async move {
            // A claim still reserving is unknown to core until its reservation commits.
            for attempt in 0..2 {
                match guard.cancel().await {
                    Ok((result, _)) if result.unknown && attempt == 0 => sleep(Duration::from_millis(100)).await,
                    _ => return,
                }
            }
        });
    }
}

impl ClaimGuard {
    /// Withdraws the claim, or with a `failure`, abandons its move.
    async fn cancel(&self) -> io::Result<(WithdrawResult, Option<Position>)> {
        let operation = &self.claim.operation_id;
        match &self.failure {
            Some(reason) => {
                let arguments = AbandonMoveArguments { reason: reason.clone() };
                self.platform.call("abandon_move", operation, &arguments, RPC_TIMEOUT).await
            }
            None => self.platform.call("withdraw", operation, &(), RPC_TIMEOUT).await,
        }
    }
}

/// Where core placed a claim's player.
struct Assignment {
    /// The claim as hooks and commands name it.
    identity: ClaimIdentity,
    /// The session the claim placed the player in.
    #[cfg(feature = "test-support")]
    session: String,
    protocol: i32,
    preparation: PlayerPreparation,
}

impl Assignment {
    /// A login's membership begins with its claim; a move keeps its source's.
    fn new(assigned: ClaimAssignment, claim: &ClaimRequest) -> io::Result<Self> {
        let delivery =
            assigned.generation.as_ref().map(generation).ok_or_else(|| invalid_data("missing generation"))?;
        let membership = claim.source.as_ref().map_or(delivery, |source| source.membership_generation);
        Ok(Self {
            identity: ClaimIdentity {
                operation_id: claim.operation_id.clone(),
                proxy_id: claim.proxy_id.clone(),
                membership_generation: membership,
                delivery_generation: delivery,
            },
            #[cfg(feature = "test-support")]
            session: assigned.session,
            protocol: assigned.protocol,
            preparation: PlayerPreparation {
                operation_id: claim.operation_id.clone(),
                endpoint: assigned.endpoint,
                capability: assigned.capability,
            },
        })
    }
}

/// Claims `guard`'s login or queued move. `None` means the login's deployment no longer accepts logins, so it must be
/// routed again; it reserved nothing.
async fn claim(guard: &ClaimGuard) -> io::Result<Option<Assignment>> {
    let arguments = ClaimArguments { login: guard.claim.source.is_none().then(|| login(&guard.claim)) };
    let operation = &guard.claim.operation_id;
    let (result, _): (ClaimResult, _) = guard.platform.call("claim", operation, &arguments, WAIT_TIMEOUT).await?;
    match result.outcome {
        Some(Outcome::Assignment(assigned)) => Assignment::new(assigned, &guard.claim).map(Some),
        Some(Outcome::Refusal(refusal)) => match ClaimRefusal::try_from(refusal) {
            Ok(ClaimRefusal::RouteAgain) => Ok(None),
            Ok(ClaimRefusal::AlreadyConnected) => {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "You are already connected."))
            }
            _ => Err(invalid_data("unknown claim refusal")),
        },
        None => Err(invalid_data("missing claim outcome")),
    }
}

fn login(claim: &ClaimRequest) -> GatewayLogin {
    GatewayLogin {
        connection_id: claim.connection_id.clone(),
        player: claim.identity.as_ref().map(|identity| PlayerIdentity {
            uuid: identity.uuid.clone(),
            username: identity.username.clone(),
            properties: identity
                .properties
                .iter()
                .map(|property| PlayerProperty {
                    name: property.name.clone(),
                    value: property.value.clone(),
                    signature: property.signature.clone(),
                })
                .collect(),
        }),
        demand: claim.demand.as_ref().map(|demand| SessionDemand {
            key: demand.key.clone(),
            session_type: demand.session_type.clone(),
            machine_profile: demand.machine_profile.clone(),
        }),
        deployment: claim.deployment.clone(),
    }
}

/// Serves a login routed and claimed through the release `current` names, routing it again whenever that release
/// becomes obsolete before control reserves it.
pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    authenticated: Authenticated<S>,
    current: &Retarget,
    deadline: Duration,
) -> io::Result<()> {
    let login = login_claim(&authenticated.profile, current.platform().connection_id());
    let destination = claim_destination(&login, current);
    let (mut authenticated, mut settings, (mut guard, mut assignment)) =
        configuration::wait_for_destination(authenticated, destination, deadline.min(WAIT_TIMEOUT)).await?;
    let platform = guard.platform.clone();
    let mut lifecycle = Lifecycle::new(platform.clone());
    let mut commands = commands::Commands::new(&platform).await?;
    loop {
        commands.bind(&guard.claim, &assignment.identity)?;
        let mut internal = timeout(deadline.min(WAIT_TIMEOUT), open(&assignment, &guard, &authenticated, &settings))
            .await
            .map_err(io::Error::other)??;
        timeout(
            deadline.min(WAIT_TIMEOUT),
            Box::pin(configuration::relay(&mut authenticated.transport, &mut internal, &mut settings)),
        )
        .await
        .map_err(io::Error::other)??;
        let identity = assignment.identity.clone();
        let arrival = relay::until(
            &mut authenticated.transport,
            &mut internal,
            &mut settings,
            Box::pin(arrive(&guard, identity.clone())),
            true,
            Some(&mut commands),
        )
        .await?;
        if let Err(error) = arrival {
            let _ =
                configuration::disconnect(&mut authenticated.transport, 0x20, "Server temporarily unavailable").await;
            return Err(error);
        }
        lifecycle.arrived(&guard.claim)?;
        commands.arrived();
        tracing::info!(operation = %guard.claim.operation_id, player = %guard.claim.identity.as_ref().map_or("", |identity| identity.uuid.as_str()), "player arrived in managed session");
        let next = relay::until(
            &mut authenticated.transport,
            &mut internal,
            &mut settings,
            Box::pin(next_move(&guard, &identity, authenticated.protocol_version)),
            true,
            Some(&mut commands),
        )
        .await??;
        commands.configuration();
        timeout(
            Duration::from_secs(10),
            relay::start_configuration(&mut authenticated.transport, &mut internal, &mut settings, Some(&commands)),
        )
        .await
        .map_err(io::Error::other)??;
        check_move(&platform, &next.0.claim).await?;
        // The client's acknowledgment fences all remaining source PLAY input.
        if let Err(error) = withdraw(&guard).await {
            let _ = configuration::disconnect(&mut authenticated.transport, 0x02, "Session move unavailable").await;
            return Err(error);
        }
        lifecycle.cutover(&next.0.claim);
        guard.armed = false;
        drop(internal);
        (guard, assignment) = next;
    }
}

/// Routes and claims `login` through the current release. A failed routing through a release that is no longer
/// current, which a reload may have released meanwhile, is routed again, as is a claim core refuses because its
/// release no longer accepts logins. The caller bounds the retries.
async fn claim_destination(login: &ClaimRequest, current: &Retarget) -> io::Result<(ClaimGuard, Assignment)> {
    loop {
        let platform = current.platform();
        let deployment = &platform.target.deployment;
        let mut claim =
            ClaimRequest { proxy_id: platform.proxy_id.clone(), deployment: deployment.clone(), ..login.clone() };
        claim.demand = match platform.route_claim(&claim).await {
            Ok(demand) => Some(demand),
            Err(_) if current.platform().target.deployment != *deployment => continue,
            Err(error) => return Err(error),
        };
        // Construct before sending: cancellation must cover a claim whose reply was lost.
        let mut guard = ClaimGuard { platform: platform.clone(), claim, armed: true, failure: None };
        if let Some(assignment) = self::claim(&guard).await? {
            return Ok((guard, assignment));
        }
        guard.armed = false;
        sleep(Duration::from_millis(100)).await;
    }
}

fn login_claim(profile: &chunk_protocol::versions::v26_2::LoginSuccess, connection_id: String) -> ClaimRequest {
    ClaimRequest {
        operation_id: uuid::Uuid::new_v4().to_string(),
        connection_id,
        identity: Some(gameplay::identity(profile)),
        ..ClaimRequest::default()
    }
}

async fn open<S>(
    assignment: &Assignment,
    guard: &ClaimGuard,
    authenticated: &Authenticated<S>,
    settings: &chunk_protocol::versions::v26_2::ConfigurationClientInformation,
) -> io::Result<Transport<tokio::net::TcpStream>> {
    if assignment.protocol != authenticated.protocol_version {
        return Err(invalid_data("destination protocol differs from client"));
    }
    activate(guard).await?;
    gameplay::login(authenticated, settings, assignment.preparation.clone()).await
}

/// Records admission intent. A roster member waits for the rest of its group; the caller bounds the wait.
async fn activate(guard: &ClaimGuard) -> io::Result<()> {
    loop {
        let operation = &guard.claim.operation_id;
        let (result, _): (ActivateResult, _) = guard.platform.call("activate", operation, &(), RPC_TIMEOUT).await?;
        if !result.waiting {
            return Ok(());
        }
        sleep(Duration::from_millis(250)).await;
    }
}

async fn withdraw(source: &ClaimGuard) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        match source.cancel().await {
            Ok((result, _)) if !result.unknown => return Ok(()),
            Ok(_) => return Err(invalid_data("source claim unknown")),
            Err(error)
                if moves::transient(&error)
                    || moves::rpc_error(&error).is_some_and(|status| status.code() == tonic::Code::Unknown) => {}
            Err(error) => return Err(error),
        }
        // Repeating the withdrawal names the same claim under its operation ID.
        sleep(Duration::from_millis(100)).await;
    }
    Err(io::Error::new(io::ErrorKind::TimedOut, "source withdrawal unresolved"))
}

/// Withdraws each claim this gateway's view shows that another process under its ID created, retrying each until core
/// takes its withdrawal. `ready` resolves once every such claim in the first live view is withdrawing or gone; later
/// ones, such as those of calls a replaced process made before it lost the ID, are withdrawn as they appear. Returns
/// once the view fails, as when another process takes the ID over. Control completes a withdrawal whose JVM has yet to
/// confirm it.
pub(super) async fn withdraw_inherited(platform: &Platform, ready: oneshot::Sender<()>) -> io::Error {
    let (mut ready, mut first, mut settled) = (Some(ready), None, None);
    loop {
        let inherited = platform.claims(|view| {
            let inherited = view.inherited();
            let first = first.get_or_insert_with(|| inherited.clone());
            let waiting = ready.is_some() && first.is_disjoint(&inherited);
            (view.passed(settled.as_ref()) && (waiting || !inherited.is_empty())).then_some(inherited)
        });
        let inherited = match inherited.await {
            Ok(inherited) => inherited,
            Err(error) => return error,
        };
        if first.as_ref().is_some_and(|first| first.is_disjoint(&inherited))
            && let Some(ready) = ready.take()
        {
            let _ = ready.send(());
        }
        if !inherited.is_empty() {
            tracing::info!(claims = inherited.len(), "withdrawing claims another gateway process left open");
        }
        let mut failed = false;
        for operation in inherited {
            match platform.call::<WithdrawResult>("withdraw", &operation, &(), RPC_TIMEOUT).await {
                Ok((_, position)) if position.as_ref().map(generation) > settled.as_ref().map(generation) => {
                    settled = position;
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, operation, "inherited claim withdrawal unresolved; retrying");
                    failed = true;
                }
            }
        }
        if failed {
            sleep(Duration::from_secs(1)).await;
        }
    }
}

/// Waits until the claim view shows the claim arrived.
async fn arrive(guard: &ClaimGuard, identity: ClaimIdentity) -> io::Result<()> {
    let arrived = guard.platform.claims(|view| {
        if view.released(&identity) {
            return Some(false);
        }
        match view.claim(&identity).and_then(|claim| ClaimPhase::try_from(claim.phase).ok())? {
            ClaimPhase::Arrived => Some(true),
            ClaimPhase::Withdrawing => Some(false),
            _ => None,
        }
    });
    match timeout(Duration::from_secs(20), arrived).await {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(io::Error::other("claim withdrawn during activation")),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(io::Error::new(io::ErrorKind::TimedOut, "session arrival timed out")),
    }
}
