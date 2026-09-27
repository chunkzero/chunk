mod select;

use chunk_proto::v1::{
    Assignment, ClaimPhase, ClaimRequest, ConfigurationRequest, ConfigurationResponse, DeploymentRef, PlayerDelivery,
    PlayerPreparation, PlayerRef, SessionRef, gameplay_client::GameplayClient,
};
use prost::Message;
use std::time::Duration;
use tonic::transport::Channel;

use crate::{
    Control, Error, Result, RuntimeConnection,
    client::{auth, channel},
    state::{Capacity, Claim, Generation, HostState, Phase, State},
};
use select::select_session;
pub(crate) use select::{select_room, validate_demand};

impl Control {
    /// Reserves capacity durably, coalesces demand, and prepares a non-active delivery.
    /// # Errors
    /// Rejects duplicate membership, changed operations, unknown session types and unresolved hosts. Reports `Busy`
    /// until recovery has fenced surviving JVMs, even for a restored claim: the log may have lost its cancellation.
    pub async fn claim(&self, request: ClaimRequest) -> Result<Assignment> {
        validate(&request)?;
        let operation = self.operation(&request.operation_id)?;
        // Checked before recovery's JVM calls, and again where the reservation commits.
        if self.draining.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::Invalid("control draining"));
        }
        self.admit().await?;
        let unavailable = self.unavailable()?;
        self.update(|state| {
            if self.draining.load(std::sync::atomic::Ordering::Acquire) {
                return Err(Error::Invalid("control draining"));
            }
            reserve(state, &request, &unavailable)
        })?;
        self.wake_capacity();
        let _guard = operation.lock().await;
        let state = self.state()?;
        let claim = state.claims.get(&request.operation_id).ok_or(Error::Invalid("unknown claim"))?;
        claim.matches(&request)?;
        if matches!(claim.phase, Phase::Withdrawing | Phase::Released) {
            return Err(Error::Invalid("claim closed"));
        }
        let session = state.sessions.get(&claim.session).ok_or(Error::Invalid("missing session"))?;
        let deployment = &state.host_release(&session.host)?.deployment;
        let runtime = self.runtime(&session.host).await?;
        // A JVM registered over sync is reached through its topic instead of its gameplay endpoint.
        let channel = if runtime.over_sync() { None } else { Some(channel(&runtime).await?) };
        let config = match &channel {
            Some(channel) => configuration(deployment, &runtime, channel.clone()).await?,
            None => self.jvm_configuration(deployment, &runtime)?,
        };
        if let Some(bytes) = &claim.assignment {
            let mut assignment = Assignment::decode(bytes.as_slice())?;
            assignment.configuration = Some(config);
            return Ok(assignment);
        }
        // Control's desired state already asks the JVM for this session, and its topic for this delivery.
        self.session_ready(&session.host, &runtime, &claim.session).await?;
        let mut delivery = delivery(deployment, &runtime, &config, claim, &request);
        let preparation = match channel {
            Some(channel) => prepare(&runtime, channel, &delivery).await?,
            None => self.prepared_over_sync(&runtime, &request.operation_id, claim.generation).await?,
        };
        delivery.identity = None;
        let assignment = Assignment {
            claim: Some(claim.identity(&request.operation_id)),
            phase: ClaimPhase::Reserved as i32,
            delivery: Some(delivery),
            configuration: Some(config),
            preparation: Some(preparation),
        };
        self.update(|state| {
            // A host released while preparing never gets a new prepared claim, which only its release would end.
            if state.released(&session.host) {
                return Err(Error::Stopped);
            }
            let claim = state.claims.get_mut(&request.operation_id).ok_or(Error::Invalid("unknown claim"))?;
            if claim.phase != Phase::Reserved {
                return Err(Error::Invalid("claim no longer reserved"));
            }
            let mut persisted = assignment.clone();
            persisted.configuration = None;
            claim.assignment = Some(persisted.encode_to_vec());
            Ok(())
        })?;
        Ok(assignment)
    }

    /// Waits until `id`'s capacity is ready and its runtime has registered. Starts nothing: the capacity executor
    /// makes every host call.
    /// # Errors
    /// Reports `Stopped` once the host's capacity is released, and `Unresolved` after 35 seconds.
    pub(crate) async fn runtime(&self, id: &str) -> Result<RuntimeConnection> {
        let mut positions = self.subscribe();
        let ready = async {
            loop {
                let state = self.state()?;
                let host = state.hosts.get(id).ok_or(Error::Invalid("unknown host"))?;
                if host.capacity == Capacity::Released {
                    return Err(Error::Stopped);
                }
                if host.capacity == Capacity::Ready
                    && let Some(runtime) = self.host.connection(id)
                {
                    if !runs_host(&state, &runtime, host) {
                        return Err(Error::Invalid("host returned incompatible runtime"));
                    }
                    return Ok(runtime);
                }
                positions.changed().await.map_err(|_| Error::Unresolved("control stopped"))?;
            }
        };
        tokio::time::timeout(Duration::from_secs(35), ready)
            .await
            .unwrap_or(Err(Error::Unresolved("app did not become ready within 35 seconds")))
    }
}

/// Whether `runtime` runs `host`'s app and profile under its release, and that release's artifact. A host whose release
/// a restore lost is its JVM's by the launch record its host adopted it with alone; no placement uses such a host, so
/// it only reports and stops.
pub(crate) fn runs_host(state: &State, runtime: &RuntimeConnection, host: &HostState) -> bool {
    let identity = &runtime.identity;
    let deployment = identity.deployment.as_ref();
    if deployment.map_or("", |deployment| deployment.deployment.as_str()) != host.release
        || identity.app_id != host.app
        || identity.machine_profile != host.profile
    {
        return false;
    }
    let Some(release) = state.releases.get(&host.release) else {
        return true;
    };
    let release = &release.release;
    deployment == Some(&release.deployment)
        && release.apps.get(&host.app).is_some_and(|app| app.sha256 == identity.artifact_digest)
}

/// The delivery `runtime` prepares for `claim`.
fn delivery(
    deployment: &DeploymentRef,
    runtime: &RuntimeConnection,
    config: &ConfigurationResponse,
    claim: &Claim,
    request: &ClaimRequest,
) -> PlayerDelivery {
    PlayerDelivery {
        deployment: Some(deployment.clone()),
        process_generation: runtime.identity.generation,
        operation_id: request.operation_id.clone(),
        session: Some(SessionRef { id: claim.session.clone() }),
        session_generation: 1,
        membership_generation: claim.membership.wire(),
        proxy_id: claim.proxy.clone(),
        connection_id: request.connection_id.clone(),
        player: Some(PlayerRef { id: claim.player.clone() }),
        owner_generation: claim.generation.wire(),
        identity: request.identity.clone(),
        protocol: config.protocol,
        runtime_id: runtime.identity.runtime_id.clone(),
    }
}

/// Asks `runtime`'s gameplay endpoint to prepare `delivery`.
async fn prepare(
    runtime: &RuntimeConnection,
    channel: Channel,
    delivery: &PlayerDelivery,
) -> Result<PlayerPreparation> {
    let preparation =
        GameplayClient::new(channel).prepare_player(auth(runtime, delivery.clone(), 3)?).await?.into_inner();
    if preparation.operation_id != delivery.operation_id
        || preparation.capability.len() != 32
        || preparation.endpoint != runtime.player_endpoint
    {
        return Err(Error::Invalid("invalid preparation"));
    }
    Ok(preparation)
}

async fn configuration(
    deployment: &DeploymentRef,
    runtime: &RuntimeConnection,
    channel: Channel,
) -> Result<ConfigurationResponse> {
    let mut gameplay = GameplayClient::new(channel).max_decoding_message_size(8 * 1024 * 1024);
    let config = gameplay
        .configuration(auth(runtime, ConfigurationRequest { deployment: Some(deployment.clone()) }, 3)?)
        .await?
        .into_inner();
    if config.deployment.as_ref() != Some(deployment)
        || config.runtime_id != runtime.identity.runtime_id
        || config.process_generation != runtime.identity.generation
    {
        return Err(Error::Invalid("configuration identity mismatch"));
    }
    Ok(config)
}

fn validate(request: &ClaimRequest) -> Result<()> {
    let identity = request.identity.as_ref().ok_or(Error::Invalid("missing authenticated identity"))?;
    let demand = request.demand.as_ref().ok_or(Error::Invalid("missing demand"))?;
    if request.encoded_len() > 65_536
        || request.operation_id.is_empty()
        || request.operation_id.len() > 128
        || request.proxy_id.is_empty()
        || request.proxy_id.len() > 128
        || request.connection_id.is_empty()
        || request.connection_id.len() > 128
        || identity.username.is_empty()
        || identity.username.len() > 16
        || !identity.username.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        || uuid::Uuid::parse_str(&identity.uuid).ok().is_none_or(|uuid| uuid.to_string() != identity.uuid)
        || demand.key.is_empty()
        || demand.key.len() > 128
    {
        return Err(Error::Invalid("invalid claim identity or demand"));
    }
    Ok(())
}

fn reserve(state: &mut State, request: &ClaimRequest, unavailable: &std::collections::BTreeSet<String>) -> Result<()> {
    if reserved(state, request)? {
        return Ok(());
    }
    let owner = owner(state, request)?;
    let (name, release) = state.placing(request)?;
    let demand = request.demand.as_ref().ok_or(Error::Invalid("demand"))?;
    let session = select_session(state, &name, &release, demand, unavailable)?;
    insert_claim(state, request, owner, session, None)
}

/// Whether `request` already has its claim. Rejects changed claims and canceled or changed moves.
pub(crate) fn reserved(state: &State, request: &ClaimRequest) -> Result<bool> {
    if let Some(intent) = state.moves.get(&request.operation_id)
        && (intent.canceled || intent.failure.is_some() || intent.request != request.encode_to_vec())
    {
        return Err(Error::Invalid("move canceled or changed"));
    }
    let Some(claim) = state.claims.get(&request.operation_id) else {
        return Ok(false);
    };
    claim.matches(request)?;
    Ok(true)
}

/// The player a new claim may own, and the membership a move continues.
pub(crate) struct Owner {
    player: String,
    membership: Option<Generation>,
}

/// Checks that `request` may take ownership of its player: a login needs an unowned player, a move needs its exact
/// arrived source with no other move pending.
pub(crate) fn owner(state: &State, request: &ClaimRequest) -> Result<Owner> {
    let player = &request.identity.as_ref().ok_or(Error::Invalid("identity"))?.uuid;
    let Some(source) = &request.source else {
        if state.players.get(player).is_some_and(|p| p.current.is_some() || p.pending.is_some()) {
            return Err(Error::Invalid(crate::ALREADY_OWNED));
        }
        return Ok(Owner { player: player.clone(), membership: None });
    };
    let owner = state.players.get(player).ok_or(Error::Invalid("missing move owner"))?;
    let previous = state.claims.get(&source.operation_id).ok_or(Error::Invalid("missing move source"))?;
    let original = ClaimRequest::decode(previous.request.as_slice())?;
    if owner.current.as_ref() != Some(&source.operation_id)
        || owner.pending.is_some()
        || previous.identity(&source.operation_id) != *source
        || previous.phase != Phase::Arrived
        || request.identity != original.identity
        || request.proxy_id != original.proxy_id
        || request.connection_id != original.connection_id
    {
        return Err(Error::Invalid("stale or competing move"));
    }
    Ok(Owner { player: player.clone(), membership: Some(previous.membership) })
}

/// Reserves a slot in `session` for `request`, whose generation is the commit applying this update.
pub(crate) fn insert_claim(
    state: &mut State,
    request: &ClaimRequest,
    owner: Owner,
    session: String,
    roster: Option<String>,
) -> Result<()> {
    let generation = Generation::PENDING;
    state.sessions.get_mut(&session).ok_or(Error::Invalid("missing selected session"))?.empty_since_ms = None;
    let player = state.players.entry(owner.player.clone()).or_default();
    if request.source.is_none() {
        player.current = Some(request.operation_id.clone());
    } else {
        player.pending = Some(request.operation_id.clone());
    }
    state.claims.insert(
        request.operation_id.clone(),
        Claim {
            request: request.encode_to_vec(),
            player: owner.player,
            proxy: request.proxy_id.clone(),
            membership: owner.membership.unwrap_or(generation),
            generation,
            session,
            phase: Phase::Reserved,
            assignment: None,
            activated: false,
            created_at_ms: crate::now_ms(),
            released_at_ms: None,
            roster,
        },
    );
    Ok(())
}
