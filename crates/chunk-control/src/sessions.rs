use std::sync::Arc;

use chunk_proto::v1::{
    ClaimIdentity, SessionCommand, SessionInventory, SessionPhase, SessionRef,
    process_control_client::ProcessControlClient,
};
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    Control, Error, Result, RuntimeConnection,
    client::{auth, channel},
    state::{Phase, State},
};

impl Control {
    /// Retires the session captured by an exact, currently arrived player claim.
    /// Existing delivery withdrawal and JVM cleanup run through normal reconciliation.
    /// # Errors
    /// Rejects stale membership, delivery generation, or ownership. A destination key grants no authority.
    pub fn finish_destination(&self, identity: &ClaimIdentity) -> Result<()> {
        self.update(|state| {
            let session = state
                .arrived_claim(identity)
                .ok_or(Error::Invalid("stale destination finish authority"))?
                .session
                .clone();
            let session = state.sessions.get_mut(&session).ok_or(Error::Invalid("missing finish session"))?;
            session.retired = true;
            session.finish_requested = true;
            Ok(())
        })
    }

    pub(crate) async fn reconcile_sessions(self: &Arc<Self>) -> Result<()> {
        let state = self.state()?;
        let permits = Arc::new(Semaphore::new(8));
        let mut tasks = JoinSet::new();
        for id in state
            .hosts
            .keys()
            .filter(|id| state.sessions.values().any(|session| &session.host == *id && !session.finished))
        {
            let id = id.clone();
            let control = self.clone();
            let permits = permits.clone();
            tasks.spawn(async move {
                let Ok(_permit) = permits.acquire_owned().await else {
                    return;
                };
                if let Err(error) = control.reconcile_host_sessions(&id).await {
                    tracing::debug!(%error, host=id, "destination disposition remains unresolved");
                }
            });
        }
        self.join_progressing_drains(tasks).await?;
        Ok(())
    }

    async fn reconcile_host_sessions(&self, host: &str) -> Result<()> {
        let state = self.state()?;
        if self.host.stopped(host) {
            return Ok(());
        }
        let Some(runtime) = self.host.connection(host) else {
            return Ok(());
        };
        self.validate_session_runtime(&state, host, &runtime)?;
        let mut client = ProcessControlClient::new(channel(&runtime).await?).max_decoding_message_size(8 * 1024 * 1024);
        let inventory = client.inventory(auth(&runtime, runtime.identity.clone(), 3)?).await?.into_inner();
        if inventory.identity.as_ref() != Some(&runtime.identity) {
            return Err(Error::Invalid("session inventory process mismatch"));
        }
        self.fence_deliveries(&runtime, &inventory).await?;
        let now = crate::now_ms();
        let finish = self.update(|state| {
            let mut finish = Vec::new();
            for (id, session) in
                state.sessions.iter_mut().filter(|(_, session)| session.host == host && !session.finished)
            {
                let empty = !state.claims.values().any(|claim| claim.session == *id && claim.phase != Phase::Released);
                if empty {
                    session.empty_since_ms.get_or_insert(now);
                } else {
                    session.empty_since_ms = None;
                }
                let observed = inventory
                    .sessions
                    .iter()
                    .find(|observed| observed.session.as_ref().is_some_and(|reference| reference.id == *id));
                if let Some(observed) = observed {
                    validate_inventory(id, session, observed)?;
                    match SessionPhase::try_from(observed.phase).map_err(|_| Error::Invalid("unknown session phase"))? {
                        SessionPhase::Ended => {
                            session.retired = true;
                            session.finish_requested = true;
                            session.finished = empty && observed.prepared == 0 && observed.attached == 0;
                        }
                        SessionPhase::Ending | SessionPhase::Failed => {
                            session.retired = true;
                            session.finish_requested = true;
                        }
                        SessionPhase::Starting | SessionPhase::Ready => {}
                        SessionPhase::Unspecified => return Err(Error::Invalid("unknown session phase")),
                    }
                }
                let destinations = self.config.contracts.destinations.as_ref();
                let policy =
                    destinations.and_then(|policies| policies.policy(&session.session_type, &session.demand_key));
                let expired = empty
                    && policy.is_some_and(|policy| {
                        session.empty_since_ms.is_some_and(|since| {
                            now.saturating_sub(since) >= u64::from(policy.empty_timeout_seconds) * 1000
                        })
                    });
                if expired || (empty && session.retired) {
                    session.retired = true;
                    session.finish_requested = true;
                }
                if session.finish_requested && !session.finished {
                    finish.push((id.clone(), session.clone()));
                }
            }
            Ok(finish)
        })?;
        for (id, session) in finish {
            let command = SessionCommand {
                identity: Some(runtime.identity.clone()),
                operation_id: format!("finish/{id}"),
                session: Some(SessionRef { id: id.clone() }),
                generation: 1,
                session_type: session.session_type.clone(),
                capacity: session.capacity,
                configuration_json: serde_json::to_vec(&session.configuration)?,
            };
            match client.finish_session(auth(&runtime, command, 10)?).await {
                Ok(response) => {
                    let observed = response.into_inner();
                    validate_inventory(&id, &session, &observed)?;
                    if observed.phase == SessionPhase::Ended as i32 && observed.prepared == 0 && observed.attached == 0
                    {
                        self.update(|state| {
                            let empty = !state
                                .claims
                                .values()
                                .any(|claim| claim.session == id && claim.phase != Phase::Released);
                            let session =
                                state.sessions.get_mut(&id).ok_or(Error::Invalid("missing finished session"))?;
                            session.finished = empty;
                            Ok(())
                        })?;
                    }
                }
                Err(error) => tracing::debug!(%error, session=id,"session finish will be reconciled"),
            }
        }
        Ok(())
    }

    fn validate_session_runtime(&self, state: &State, host: &str, runtime: &RuntimeConnection) -> Result<()> {
        let expected = state.hosts.get(host).ok_or(Error::Invalid("missing host"))?;
        if !self.runs_host(runtime, expected) {
            return Err(Error::Invalid("session cleanup runtime mismatch"));
        }
        Ok(())
    }
}

fn validate_inventory(id: &str, session: &crate::state::SessionState, observed: &SessionInventory) -> Result<()> {
    if observed.session.as_ref().is_none_or(|reference| reference.id != id)
        || observed.generation != 1
        || observed.session_type != session.session_type
        || observed.capacity != session.capacity
    {
        return Err(Error::Invalid("session inventory binding mismatch"));
    }
    Ok(())
}
