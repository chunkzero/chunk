use std::time::Duration;

use crate::RuntimeConnection;
use chunk_proto::v1::{
    Assignment, ClaimPhase, ClaimRequest, ConfigurationRequest, PlayerDelivery, PlayerRef, SessionCommand,
    SessionPhase, SessionRef, gameplay_client::GameplayClient, process_control_client::ProcessControlClient,
};
use prost::Message;
use tonic::{Request, transport::Channel};

use crate::{
    Config, Control, Error, Result,
    state::{Claim, HostState, Phase, SessionState, State},
};

impl Control {
    /// Reserves capacity durably, coalesces demand, and prepares a non-active delivery.
    /// # Errors
    /// Rejects duplicate membership, changed operations, unknown session types and unresolved hosts.
    pub async fn claim(&self, request: ClaimRequest) -> Result<Assignment> {
        validate(&request)?;
        let unavailable = self.unavailable()?;
        self.update(|state| {
            if self.draining.load(std::sync::atomic::Ordering::Acquire) {
                return Err(Error::Invalid("control draining"));
            }
            reserve(state, &self.config, &request, &unavailable)
        })?;
        let operation = self.operation(&request.operation_id)?;
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
                },
                10,
            )?)
            .await?
            .into_inner();
        if created.phase != SessionPhase::Ready as i32
            || created.generation != 1
            || created.session.as_ref().map(|s| &s.id) != Some(&claim.session)
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
            membership_generation: claim.membership_generation,
            proxy_id: claim.proxy.clone(),
            connection_id: request.connection_id.clone(),
            player: Some(PlayerRef { id: claim.player.clone() }),
            owner_generation: claim.delivery_generation,
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
        let runtime = self.host.ensure(id, &host.app, &host.profile).await?;
        if runtime.identity.deployment.as_ref() != Some(&self.config.deployment)
            || runtime.identity.machine_profile != host.profile
            || runtime.identity.artifact_digest != self.config.apps[&host.app].sha256
            || runtime.identity.app_id != host.app
        {
            return Err(Error::Invalid("host returned incompatible runtime"));
        }
        Ok(runtime)
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
    if let Some(intent) = state.moves.get(&request.operation_id)
        && (intent.canceled || intent.request != request.encode_to_vec())
    {
        return Err(Error::Invalid("move canceled or changed"));
    }
    if let Some(claim) = state.claims.get(&request.operation_id) {
        return claim.matches(request);
    }
    if state.claims.len() >= 1024 {
        return Err(Error::Capacity);
    }
    let player = &request.identity.as_ref().ok_or(Error::Invalid("identity"))?.uuid;
    if let Some(source) = &request.source {
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
    } else if state.players.get(player).is_some_and(|p| p.current.is_some() || p.pending.is_some()) {
        return Err(Error::Invalid("player already owned"));
    }
    let session = select_session(state, config, request.demand.as_ref().ok_or(Error::Invalid("demand"))?, unavailable)?;
    state.sessions.get_mut(&session).ok_or(Error::Invalid("missing selected session"))?.empty_since_ms = None;
    let owner = state.players.entry(player.clone()).or_default();
    if request.source.is_none() {
        owner.membership_generation = owner.membership_generation.checked_add(1).ok_or(Error::Capacity)?;
        owner.current = Some(request.operation_id.clone());
    } else {
        owner.pending = Some(request.operation_id.clone());
    }
    owner.delivery_generation = owner.delivery_generation.checked_add(1).ok_or(Error::Capacity)?;
    state.claims.insert(
        request.operation_id.clone(),
        Claim {
            request: request.encode_to_vec(),
            player: player.clone(),
            proxy: request.proxy_id.clone(),
            membership_generation: owner.membership_generation,
            delivery_generation: owner.delivery_generation,
            session,
            phase: Phase::Reserved,
            assignment: None,
            activated: false,
            created_at_ms: crate::now_ms(),
        },
    );
    Ok(())
}

fn select_session(
    state: &mut State,
    config: &Config,
    demand: &chunk_proto::v1::SessionDemand,
    unavailable: &std::collections::BTreeSet<String>,
) -> Result<String> {
    let spec = config.session_types.get(&demand.session_type).ok_or(Error::Invalid("unknown session type"))?;
    if !demand.machine_profile.is_empty() && demand.machine_profile != spec.machine_profile {
        return Err(Error::Invalid("session profile mismatch"));
    }
    let policy = config.destinations.as_ref().and_then(|policies| policies.policy(&demand.session_type, &demand.key));
    if policy.is_some_and(|policy| policy.destination.machine_profile != demand.machine_profile) {
        return Err(Error::Invalid("declared destination profile mismatch"));
    }
    let existing = state
        .sessions
        .iter()
        .find(|(id, session)| {
            !session.retired
                && !unavailable.contains(&session.host)
                && session.session_type == demand.session_type
                && session.demand_key == demand.key
                && state.claims.values().filter(|c| c.session == **id && c.phase != Phase::Released).count()
                    < session.capacity as usize
        })
        .map(|(id, _)| id.clone());
    let session = if let Some(id) = existing {
        id
    } else {
        if policy.is_some_and(|policy| policy.overflow == chunk_contract::DestinationOverflow::Reject)
            && state.sessions.values().any(|session| {
                !session.finished && session.session_type == demand.session_type && session.demand_key == demand.key
            })
        {
            return Err(Error::Capacity);
        }
        if state.sessions.len() >= 256 {
            return Err(Error::Capacity);
        }
        let limit = config.profiles.get(&spec.machine_profile).ok_or(Error::Invalid("missing profile"))?.max_sessions;
        let existing_host = state
            .hosts
            .iter()
            .find(|(id, host)| {
                !host.retired
                    && !unavailable.contains(*id)
                    && host.app == spec.app
                    && host.profile == spec.machine_profile
                    && state.sessions.values().filter(|s| s.host == **id && !s.finished).count() < usize::from(limit)
                    && state
                        .sessions
                        .values()
                        .filter(|s| s.host == **id && !s.finished)
                        .map(|s| s.capacity)
                        .sum::<u32>()
                        + spec.capacity
                        <= 128
            })
            .map(|(id, _)| id.clone());
        let host = if let Some(id) = existing_host {
            id
        } else {
            if state.hosts.values().filter(|h| !h.retired).count() >= usize::from(config.max_processes) {
                return Err(Error::Capacity);
            }
            let id = uuid::Uuid::new_v4().to_string();
            state.hosts.insert(
                id.clone(),
                HostState { app: spec.app.clone(), profile: spec.machine_profile.clone(), retired: false },
            );
            id
        };
        let id = uuid::Uuid::new_v4().to_string();
        state.sessions.insert(
            id.clone(),
            SessionState {
                empty_since_ms: None,
                finish_requested: false,
                finished: false,
                host,
                session_type: demand.session_type.clone(),
                demand_key: demand.key.clone(),
                capacity: spec.capacity,
                retired: false,
            },
        );
        id
    };
    Ok(session)
}

pub(crate) async fn channel(runtime: &RuntimeConnection) -> Result<Channel> {
    let endpoint = runtime.endpoint.strip_prefix("http://").ok_or(Error::Invalid("local runtime URL"))?;
    let address: std::net::SocketAddr = endpoint.parse().map_err(|_| Error::Invalid("runtime address"))?;
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(Error::Invalid("runtime must be loopback"));
    }
    Channel::from_shared(runtime.endpoint.clone())
        .map_err(|_| Error::Invalid("runtime URL"))?
        .connect_timeout(Duration::from_secs(3))
        .connect()
        .await
        .map_err(|_| Error::Unresolved("runtime connection unavailable"))
}

pub(crate) fn auth<T>(runtime: &RuntimeConnection, body: T, seconds: u64) -> Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {}", runtime.token).parse().map_err(|_| Error::Invalid("runtime credential"))?,
    );
    request.set_timeout(Duration::from_secs(seconds));
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn placement_groups_only_matching_apps_and_profiles() {
        let mut state = State::default();
        let mut config = Config {
            destinations: None,
            apps: BTreeMap::new(),
            deployment: chunk_proto::v1::DeploymentRef::default(),
            artifact_digest: "release".into(),
            profiles: BTreeMap::from([
                ("small".into(), crate::MachineProfile { memory_mib: 512, max_sessions: 4 }),
                ("large".into(), crate::MachineProfile { memory_mib: 1024, max_sessions: 4 }),
            ]),
            session_types: BTreeMap::new(),
            max_processes: 4,
        };
        for (name, app, profile) in [
            ("lobby/default", "lobby", "small"),
            ("arena/default", "arena", "small"),
            ("arena/large", "arena", "large"),
        ] {
            config.session_types.insert(
                name.into(),
                crate::SessionType { app: app.into(), machine_profile: profile.into(), capacity: 16 },
            );
        }
        let mut selected = Vec::new();
        for (key, session_type) in [
            ("lobby", "lobby/default"),
            ("arena1", "arena/default"),
            ("arena2", "arena/default"),
            ("large", "arena/large"),
        ] {
            let session = select_session(
                &mut state,
                &config,
                &chunk_proto::v1::SessionDemand {
                    key: key.into(),
                    session_type: session_type.into(),
                    machine_profile: String::new(),
                },
                &BTreeSet::new(),
            )
            .unwrap();
            selected.push(state.sessions[&session].host.clone());
        }
        assert_ne!(selected[0], selected[1]);
        assert_eq!(selected[1], selected[2]);
        assert_ne!(selected[1], selected[3]);
        assert_eq!(state.hosts.len(), 3);
        assert!(
            select_session(
                &mut state,
                &config,
                &chunk_proto::v1::SessionDemand {
                    key: "changed".into(),
                    session_type: "arena/large".into(),
                    machine_profile: "small".into()
                },
                &BTreeSet::new()
            )
            .is_err()
        );
    }
}
