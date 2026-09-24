use std::collections::BTreeSet;

use chunk_proto::v1::SessionDemand;

use crate::{
    Config, Error, Result,
    state::{HostState, Phase, SessionState, State},
};

pub(crate) fn validate_demand(config: &Config, demand: &SessionDemand) -> Result<()> {
    if demand.key.is_empty() || demand.key.len() > 128 {
        return Err(Error::Invalid("invalid destination key"));
    }
    let spec = config.session_types.get(&demand.session_type).ok_or(Error::Invalid("unknown session type"))?;
    let policy =
        config.contracts.destinations.as_ref().and_then(|policies| policies.policy(&demand.session_type, &demand.key));
    resolve_creation(config, demand, spec, policy).map(|_| ())
}

/// Reuses a compatible session with room, or creates one on a compatible host or a new host.
pub(super) fn select_session(
    state: &mut State,
    config: &Config,
    demand: &SessionDemand,
    unavailable: &BTreeSet<String>,
) -> Result<String> {
    let spec = config.session_types.get(&demand.session_type).ok_or(Error::Invalid("unknown session type"))?;
    let policy =
        config.contracts.destinations.as_ref().and_then(|policies| policies.policy(&demand.session_type, &demand.key));
    let creation = resolve_creation(config, demand, spec, policy)?;
    if let Some(id) = reuse_session(state, demand, &creation, unavailable) {
        return Ok(id);
    }
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
    let host = place_host(state, config, &spec.app, &creation, unavailable)?;
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
            capacity: creation.capacity,
            configuration: creation.configuration,
            retired: false,
        },
    );
    Ok(id)
}

fn reuse_session(
    state: &State,
    demand: &SessionDemand,
    creation: &Creation,
    unavailable: &BTreeSet<String>,
) -> Option<String> {
    state
        .sessions
        .iter()
        .find(|(id, session)| {
            !session.retired
                && !unavailable.contains(&session.host)
                && session.session_type == demand.session_type
                && session.demand_key == demand.key
                && session.capacity == creation.capacity
                && session.configuration == creation.configuration
                && state.hosts.get(&session.host).is_some_and(|host| host.profile == creation.profile)
                && state.claims.values().filter(|c| c.session == **id && c.phase != Phase::Released).count()
                    < session.capacity as usize
        })
        .map(|(id, _)| id.clone())
}

fn place_host(
    state: &mut State,
    config: &Config,
    app: &str,
    creation: &Creation,
    unavailable: &BTreeSet<String>,
) -> Result<String> {
    let limit = config.profiles.get(creation.profile).ok_or(Error::Invalid("missing profile"))?.max_sessions;
    let existing = state.hosts.iter().find(|(id, host)| {
        let sessions: Vec<_> = state.sessions.values().filter(|s| s.host == **id && !s.finished).collect();
        !host.retired
            && !unavailable.contains(*id)
            && host.app == app
            && host.profile == creation.profile
            && sessions.len() < usize::from(limit)
            && sessions.iter().map(|s| s.capacity).sum::<u32>() + creation.capacity <= 128
    });
    if let Some(id) = existing.map(|(id, _)| id.clone()) {
        state.hosts.get_mut(&id).ok_or(Error::Invalid("missing host"))?.idle_since_ms = None;
        return Ok(id);
    }
    if state.hosts.values().filter(|h| !h.retired).count() >= usize::from(config.max_processes) {
        return Err(Error::Capacity);
    }
    let id = uuid::Uuid::new_v4().to_string();
    state.hosts.insert(
        id.clone(),
        HostState { app: app.into(), profile: creation.profile.into(), retired: false, idle_since_ms: None },
    );
    Ok(id)
}

struct Creation<'a> {
    profile: &'a str,
    capacity: u32,
    configuration: serde_json::Value,
}

fn resolve_creation<'a>(
    config: &Config,
    demand: &SessionDemand,
    spec: &'a crate::SessionType,
    policy: Option<&'a chunk_contract::DestinationPolicy>,
) -> Result<Creation<'a>> {
    let profile = policy.map_or(spec.machine_profile.as_str(), |policy| policy.destination.machine_profile.as_str());
    if (policy.is_some() || !demand.machine_profile.is_empty()) && demand.machine_profile != profile {
        return Err(Error::Invalid("destination profile mismatch"));
    }
    let declared = policy.and_then(|policy| policy.creation.as_ref());
    let capacity = declared.map_or(spec.capacity, |creation| creation.capacity);
    let mut configuration = declared.map_or_else(|| serde_json::json!({}), |creation| creation.configuration.clone());
    chunk_contract::validate_session_configuration(
        config.contracts.session_configurations.as_ref(),
        &demand.session_type,
        &configuration,
    )
    .map_err(Error::Invalid)?;
    configuration.sort_all_objects();
    Ok(Creation { profile, capacity, configuration })
}

#[cfg(test)]
mod tests;
