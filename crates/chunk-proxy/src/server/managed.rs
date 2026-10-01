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

use chunk_proto::sync::v1::{
    AbandonMoveArguments, ActivateResult, ClaimArguments, ClaimAssignment, ClaimPhase, ClaimRefusal, ClaimResult,
    GatewayLogin, PlayerSetup, Position, ReconnectArguments, ReservationResult, SessionDemand, WithdrawResult,
    claim_result::Outcome,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Notify, oneshot},
    time::{Instant, sleep, timeout},
};

use super::{
    Retarget,
    authentication::Authenticated,
    claim::{Claim, ClaimIdentity},
    configuration, gameplay,
    platform::{Lifecycle, Platform, RPC_TIMEOUT, generation},
    transport::{Transport, invalid_data},
};

use moves::{check_move, next_move};

const WAIT_TIMEOUT: Duration = Duration::from_secs(45);
/// How often a gateway tells core how many connections it holds.
const ACTIVE_EVERY: Duration = Duration::from_secs(1);

struct ClaimGuard {
    platform: Platform,
    claim: Claim,
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
    /// The deployment of the release the session runs.
    deployment: String,
    /// The session's destination.
    destination: SessionDemand,
    /// The destination JVM's player listener.
    endpoint: String,
    setup: PlayerSetup,
}

impl Assignment {
    fn new(assigned: ClaimAssignment, claim: &Claim) -> io::Result<Self> {
        let delivery =
            assigned.generation.as_ref().map(generation).ok_or_else(|| invalid_data("missing generation"))?;
        Ok(Self {
            identity: ClaimIdentity { operation_id: claim.operation_id.clone(), delivery_generation: delivery },
            #[cfg(feature = "test-support")]
            session: assigned.session,
            protocol: assigned.protocol,
            deployment: assigned.deployment,
            destination: assigned.destination.unwrap_or_default(),
            endpoint: assigned.endpoint,
            setup: PlayerSetup { operation_id: claim.operation_id.clone(), capability: assigned.capability },
        })
    }
}

/// Claims `guard`'s login or queued move. `None` means the deployment that admitted it is no longer current, so it must
/// be admitted again; it reserved nothing.
async fn claim(guard: &ClaimGuard) -> io::Result<Option<Assignment>> {
    let arguments = ClaimArguments {
        login: guard.claim.source.is_none().then(|| login(&guard.claim)),
        deployment: guard.claim.deployment.clone(),
    };
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

fn login(claim: &Claim) -> GatewayLogin {
    GatewayLogin {
        connection_id: claim.connection_id.clone(),
        player: Some(claim.player.clone()),
        demand: Some(claim.demand.clone()),
        deployment: claim.deployment.clone(),
        reconnect_session: claim.reconnect.clone(),
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
    let mut platform = guard.platform.clone();
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
        tracing::info!(operation = %guard.claim.operation_id, player = %guard.claim.player.uuid, "player arrived in managed session");
        let next = relay::until(
            &mut authenticated.transport,
            &mut internal,
            &mut settings,
            Box::pin(next_move(&guard, &identity, authenticated.protocol_version, current)),
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
        if guard.platform.target.deployment != platform.target.deployment {
            platform = guard.platform.clone();
            lifecycle.rebind(platform.clone());
            commands = commands::Commands::new(&platform).await?;
        }
    }
}

/// Routes and claims `login` through the current release. A login whose player may return to the session they left on
/// an earlier release is admitted there, as that session's destination, without consulting the current release's hooks,
/// and claimed for exactly that session; otherwise it is routed and placed with the current release. A failed routing
/// through a release that is no longer current, which a reload may have released meanwhile, is routed again, as is a
/// claim core refuses because its release no longer accepts logins or the session no longer takes the player back. The
/// caller bounds the retries.
async fn claim_destination(login: &Claim, current: &Retarget) -> io::Result<(ClaimGuard, Assignment)> {
    let mut login = login.clone();
    loop {
        let platform = current.platform();
        let mut guard = match reconnect(&platform, &login).await? {
            Reconnect::Admitted(guard) => *guard,
            Reconnect::Declined => {
                login.decline_reconnect = true;
                continue;
            }
            Reconnect::None => {
                let deployment = &platform.target.deployment;
                let mut claim = Claim { deployment: deployment.clone(), ..login.clone() };
                claim.demand = match platform.route_claim(&claim).await {
                    Ok(demand) => demand,
                    Err(_) if current.platform().target.deployment != *deployment => continue,
                    Err(error) => return Err(error),
                };
                // Construct before sending: cancellation must cover a claim whose reply was lost.
                ClaimGuard { platform: platform.clone(), claim, armed: true, failure: None }
            }
        };
        let Some(assignment) = self::claim(&guard).await? else {
            guard.armed = false;
            sleep(Duration::from_millis(100)).await;
            continue;
        };
        return Ok((guard, assignment));
    }
}

/// What asking core whether `login`'s player may return to an earlier release's session came to.
enum Reconnect {
    /// There is no such session, or the login declines it.
    None,
    /// The session's deployment denied the login, which then goes to the current release.
    Declined,
    /// The session's deployment admitted the login, which core can now claim there.
    Admitted(Box<ClaimGuard>),
}

async fn reconnect(platform: &Platform, login: &Claim) -> io::Result<Reconnect> {
    if login.decline_reconnect {
        return Ok(Reconnect::None);
    }
    let arguments = ReconnectArguments { player: login.player.uuid.clone() };
    let (target, _): (ReservationResult, _) =
        platform.call("reconnect", &login.operation_id, &arguments, RPC_TIMEOUT).await?;
    let Some(destination) = target.destination.filter(|_| !target.deployment.is_empty()) else {
        return Ok(Reconnect::None);
    };
    let placed = platform.bind(&target.deployment);
    let claim =
        Claim { deployment: target.deployment, demand: destination, reconnect: target.session, ..login.clone() };
    match placed.admit_login(&claim).await {
        Ok(()) => Ok(Reconnect::Admitted(Box::new(ClaimGuard { platform: placed, claim, armed: true, failure: None }))),
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Ok(Reconnect::Declined),
        Err(error) => Err(error),
    }
}

fn login_claim(profile: &chunk_protocol::versions::v26_2::LoginSuccess, connection_id: String) -> Claim {
    Claim {
        operation_id: uuid::Uuid::new_v4().to_string(),
        connection_id,
        player: gameplay::identity(profile),
        ..Claim::default()
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
    gameplay::login(authenticated, settings, &assignment.endpoint, assignment.setup.clone()).await
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

/// This gateway's connections, which core hears of every [`ACTIVE_EVERY`].
#[derive(Default)]
pub(super) struct Connected {
    activity: chunk_service::Activity,
    arrived: Notify,
}

impl Connected {
    /// Runs `connection` as one of these connections.
    pub fn track<F: Future>(&self, connection: F) -> impl Future<Output = F::Output> + use<F> {
        let busy = self.activity.begin();
        self.arrived.notify_one();
        async move {
            let _busy = busy;
            connection.await
        }
    }

    /// Tells core how many connections this gateway holds every [`ACTIVE_EVERY`], and at once when one arrives after
    /// a report of none. Until core takes a report, one that came and went since the last report it took counts too.
    async fn report(&self, platform: &Platform) -> ! {
        let (mut reported, mut idle) = (0, false);
        loop {
            let observed = self.activity.observe();
            let open = u32::try_from(observed.in_flight).unwrap_or(u32::MAX);
            let connections = open.max(u32::from(observed.changes != reported));
            match platform.active(connections).await {
                Ok(()) => (reported, idle) = (observed.changes, connections == 0),
                Err(error) => tracing::debug!(%error, "core didn't hear how many connections this gateway holds"),
            }
            if idle {
                tokio::select! {
                    () = sleep(ACTIVE_EVERY) => {}
                    () = self.arrived.notified() => {}
                }
            } else {
                sleep(ACTIVE_EVERY).await;
            }
        }
    }
}

/// Withdraws the claims other processes left open, as [`withdraw_inherited`] does, while telling core of `connected`.
pub(super) async fn follow(platform: &Platform, ready: oneshot::Sender<()>, connected: &Connected) -> io::Error {
    tokio::select! {
        error = withdraw_inherited(platform, ready) => error,
        never = connected.report(platform) => match never {},
    }
}

/// Withdraws each claim this gateway's view shows that another process under its ID created, retrying each until core
/// takes its withdrawal. `ready` resolves once every such claim in the first live view is withdrawing or gone; later
/// ones, such as those of calls a replaced process made before it lost the ID, are withdrawn as they appear. Returns
/// once the view fails, as when another process takes the ID over. Control completes a withdrawal whose JVM has yet to
/// confirm it.
async fn withdraw_inherited(platform: &Platform, ready: oneshot::Sender<()>) -> io::Error {
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
