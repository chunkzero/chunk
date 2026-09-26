use std::sync::Arc;

use chunk_proto::v1::{ClaimIdentity, SessionInventory, SessionPhase};
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{
    Control, Error, Result, RuntimeConnection,
    state::{Phase, SessionState, State},
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

    /// Reapplies what `host`'s JVM last reported, fences deliveries the log does not own, and asks the JVM to end
    /// sessions that emptied past their timeout or retired.
    pub(crate) async fn reconcile_host_sessions(&self, host: &str) -> Result<()> {
        let state = self.state()?;
        if state.released(host) {
            return Ok(());
        }
        let Some(runtime) = self.host.connection(host) else {
            return Ok(());
        };
        self.validate_session_runtime(&state, host, &runtime)?;
        let Some(report) = self.links.report(host, &runtime.identity) else {
            return Ok(());
        };
        // Unfenced deliveries are retried on the next pass.
        self.fence_deliveries(&runtime, &report).await?;
        let now = crate::now_ms();
        self.update(|state| {
            self.reapply(state, host, &runtime.identity)?;
            let open: std::collections::BTreeSet<_> = state
                .claims
                .values()
                .filter(|claim| claim.phase != Phase::Released)
                .map(|claim| claim.session.clone())
                .collect();
            for (id, session) in
                state.sessions.iter_mut().filter(|(_, session)| session.host == host && !session.finished)
            {
                let empty = !open.contains(id);
                if empty {
                    session.empty_since_ms.get_or_insert(now);
                } else {
                    session.empty_since_ms = None;
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
            }
            Ok(())
        })
    }

    fn validate_session_runtime(&self, state: &State, host: &str, runtime: &RuntimeConnection) -> Result<()> {
        let expected = state.hosts.get(host).ok_or(Error::Invalid("missing host"))?;
        if !self.runs_host(runtime, expected) {
            return Err(Error::Invalid("session cleanup runtime mismatch"));
        }
        Ok(())
    }
}

/// Records the phase a JVM reported for one of `host`'s sessions. A session that ended, is ending or failed is retired;
/// one that ended or failed with no open claim or delivery is finished, which frees its capacity.
pub(crate) fn apply(state: &mut State, host: &str, observed: &SessionInventory) {
    let Some(id) = observed.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let empty = !state.claims.values().any(|claim| claim.session == id && claim.phase != Phase::Released);
    let Some(session) = state.sessions.get_mut(&id).filter(|session| session.host == host && !session.finished) else {
        return;
    };
    if !matches(&id, session, observed) {
        tracing::debug!(session = id, "ignoring a mismatched session report");
        return;
    }
    match SessionPhase::try_from(observed.phase) {
        Ok(SessionPhase::Ended | SessionPhase::Failed) => {
            session.retired = true;
            session.finish_requested = true;
            session.finished = empty && observed.prepared == 0 && observed.attached == 0;
        }
        Ok(SessionPhase::Ending) => {
            session.retired = true;
            session.finish_requested = true;
        }
        _ => {}
    }
}

/// Whether `observed` reports the session control recorded as `id`.
pub(crate) fn matches(id: &str, session: &SessionState, observed: &SessionInventory) -> bool {
    observed.session.as_ref().is_some_and(|reference| reference.id == id)
        && observed.generation == 1
        && observed.session_type == session.session_type
        && observed.capacity == session.capacity
}
