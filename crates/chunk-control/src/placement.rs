mod select;

use chunk_proto::v1::{
    Assignment, ClaimPhase, ClaimRequest, ConfigurationRequest, PlayerDelivery, PlayerRef, SessionCommand,
    SessionPhase, SessionRef, gameplay_client::GameplayClient, process_control_client::ProcessControlClient,
};
use prost::Message;
use tonic::transport::Channel;

use crate::{
    Config, Control, Error, Result, RuntimeConnection,
    client::{auth, channel},
    state::{Claim, Generation, HostState, Phase, State},
};
use select::select_session;
pub(crate) use select::validate_demand;

impl Control {
    /// Reserves capacity durably, coalesces demand, and prepares a non-active delivery.
    /// # Errors
    /// Rejects duplicate membership, changed operations, unknown session types and unresolved hosts.
    pub async fn claim(&self, request: ClaimRequest) -> Result<Assignment> {
        validate(&request)?;
        let operation = self.operation(&request.operation_id)?;
        let unavailable = self.unavailable()?;
        self.update(|state| {
            if self.draining.load(std::sync::atomic::Ordering::Acquire) {
                return Err(Error::Invalid("control draining"));
            }
            reserve(state, &self.config, &request, &unavailable)
        })?;
        let _guard = operation.lock().await;
        let state = self.state()?;
        let claim = state.claims.get(&request.operation_id).ok_or(Error::Invalid("unknown claim"))?;
        claim.matches(&request)?;
        if matches!(claim.phase, Phase::Withdrawing | Phase::Released) {
            return Err(Error::Invalid("claim closed"));
        }
        let session = state.sessions.get(&claim.session).ok_or(Error::Invalid("missing session"))?;
        let runtime = self.runtime(&state, &session.host).await?;
        let channel = channel(&runtime).await?;
        if let Some(bytes) = &claim.assignment {
            let mut assignment = Assignment::decode(bytes.as_slice())?;
            let config = self.configuration(&runtime, channel).await?;
            assignment.configuration = Some(config);
            return Ok(assignment);
        }

        let created = ProcessControlClient::new(channel.clone())
            .create_session(auth(
                &runtime,
                SessionCommand {
                    identity: Some(runtime.identity.clone()),
                    operation_id: format!("session/{}", claim.session),
                    session: Some(SessionRef { id: claim.session.clone() }),
                    generation: 1,
                    session_type: session.session_type.clone(),
                    capacity: session.capacity,
                    configuration_json: serde_json::to_vec(&session.configuration)?,
                },
                10,
            )?)
            .await?
            .into_inner();
        if created.phase != SessionPhase::Ready as i32
            || created.generation != 1
            || created.session.as_ref().map(|s| &s.id) != Some(&claim.session)
            || created.session_type != session.session_type
            || created.capacity != session.capacity
        {
            self.update(|s| {
                s.sessions.get_mut(&claim.session).ok_or(Error::Invalid("missing session"))?.retired = true;
                Ok(())
            })?;
            return Err(Error::Unresolved("session is not ready"));
        }
        let assignment = self.prepare_assignment(&runtime, channel, claim, &request).await?;
        self.update(|state| {
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

    async fn prepare_assignment(
        &self,
        runtime: &RuntimeConnection,
        channel: Channel,
        claim: &Claim,
        request: &ClaimRequest,
    ) -> Result<Assignment> {
        let config = self.configuration(runtime, channel.clone()).await?;
        let mut gameplay = GameplayClient::new(channel);
        let mut delivery = PlayerDelivery {
            deployment: Some(self.config.deployment.clone()),
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
        };
        let preparation = gameplay.prepare_player(auth(runtime, delivery.clone(), 3)?).await?.into_inner();
        if preparation.operation_id != request.operation_id
            || preparation.capability.len() != 32
            || preparation.endpoint != runtime.player_endpoint
        {
            return Err(Error::Invalid("invalid preparation"));
        }
        delivery.identity = None;
        let assignment = Assignment {
            claim: Some(claim.identity(&request.operation_id)),
            phase: ClaimPhase::Reserved as i32,
            delivery: Some(delivery),
            configuration: Some(config),
            preparation: Some(preparation),
        };
        Ok(assignment)
    }

    async fn configuration(
        &self,
        runtime: &RuntimeConnection,
        channel: Channel,
    ) -> Result<chunk_proto::v1::ConfigurationResponse> {
        let mut gameplay = GameplayClient::new(channel).max_decoding_message_size(8 * 1024 * 1024);
        let config = gameplay
            .configuration(auth(runtime, ConfigurationRequest { deployment: Some(self.config.deployment.clone()) }, 3)?)
            .await?
            .into_inner();
        if config.deployment.as_ref() != Some(&self.config.deployment)
            || config.runtime_id != runtime.identity.runtime_id
            || config.process_generation != runtime.identity.generation
        {
            return Err(Error::Invalid("configuration identity mismatch"));
        }
        Ok(config)
    }

    pub(crate) async fn runtime(&self, state: &State, id: &str) -> Result<RuntimeConnection> {
        let host = state.hosts.get(id).ok_or(Error::Invalid("unknown host"))?;
        let runtime = match self.host.ensure(id, &host.app, &host.profile).await {
            Ok(runtime) => runtime,
            Err(error) => {
                if self.host.stopped(id)
                    && let Err(retirement) = self.update(|state| {
                        state.retire_stopped_host(id);
                        Ok(())
                    })
                {
                    tracing::error!(%retirement, host = id, "cannot retire stopped host");
                }
                return Err(error);
            }
        };
        if !self.runs_host(&runtime, host) {
            return Err(Error::Invalid("host returned incompatible runtime"));
        }
        Ok(runtime)
    }

    /// Whether `runtime` runs this deployment's current artifact for `host`'s app and profile.
    pub(crate) fn runs_host(&self, runtime: &RuntimeConnection, host: &HostState) -> bool {
        let identity = &runtime.identity;
        identity.deployment.as_ref() == Some(&self.config.deployment)
            && identity.app_id == host.app
            && identity.machine_profile == host.profile
            && self.config.apps.get(&host.app).is_some_and(|app| app.sha256 == identity.artifact_digest)
    }
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

fn reserve(
    state: &mut State,
    config: &Config,
    request: &ClaimRequest,
    unavailable: &std::collections::BTreeSet<String>,
) -> Result<()> {
    if reserved(state, request)? {
        return Ok(());
    }
    let owner = owner(state, request)?;
    let session = select_session(state, config, request.demand.as_ref().ok_or(Error::Invalid("demand"))?, unavailable)?;
    insert_claim(state, request, owner, session)
}

/// Whether `request` already has its claim. Rejects changed claims and canceled or changed moves.
fn reserved(state: &State, request: &ClaimRequest) -> Result<bool> {
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
struct Owner {
    player: String,
    membership: Option<Generation>,
}

/// Checks that `request` may take ownership of its player: a login needs an unowned player, a move needs its exact
/// arrived source with no other move pending.
fn owner(state: &State, request: &ClaimRequest) -> Result<Owner> {
    let player = &request.identity.as_ref().ok_or(Error::Invalid("identity"))?.uuid;
    let Some(source) = &request.source else {
        if state.players.get(player).is_some_and(|p| p.current.is_some() || p.pending.is_some()) {
            return Err(Error::Invalid("player already owned"));
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

/// Reserves a slot in `session` for `request`, created by the commit applying this update.
fn insert_claim(state: &mut State, request: &ClaimRequest, owner: Owner, session: String) -> Result<()> {
    let generation = state.next_generation()?;
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
        },
    );
    Ok(())
}
