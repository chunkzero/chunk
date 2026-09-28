use chunk_proto::{
    sync::v1::{JvmSessionPhase, JvmSessionStatus},
    v1::ClaimIdentity,
};

use crate::{
    Control, Error, Result,
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

    pub(crate) fn reconcile_sessions(&self) -> Result<()> {
        let state = self.state()?;
        for id in state
            .hosts
            .keys()
            .filter(|id| state.sessions.values().any(|session| &session.host == *id && !session.finished))
        {
            if let Err(error) = self.reconcile_host_sessions(id) {
                tracing::debug!(%error, host = id, "destination disposition remains unresolved");
            }
        }
        Ok(())
    }

    /// Reapplies what `host`'s JVM last reported, and asks the JVM to end sessions that emptied past their timeout or
    /// retired.
    fn reconcile_host_sessions(&self, host: &str) -> Result<()> {
        let state = self.state()?;
        if state.released(host) {
            return Ok(());
        }
        let Some(runtime) = self.host.connection(host) else {
            return Ok(());
        };
        let expected = state.hosts.get(host).ok_or(Error::Invalid("missing host"))?;
        if !crate::placement::runs_host(&state, &runtime, expected) {
            return Err(Error::Invalid("session cleanup runtime mismatch"));
        }
        if self.links.report(host, &runtime.identity).is_none() {
            return Ok(());
        }
        let now = crate::now_ms();
        self.update(|state| {
            self.reapply(state, host, &runtime.identity)?;
            // A host whose release a restore lost only stops, so no destination policy applies to it.
            let release = state.host_release(host).ok().cloned();
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
                let destinations = release.as_ref().and_then(|release| release.contracts.destinations.as_ref());
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
}

/// Records the phase a JVM reported for one of `host`'s sessions. A session that ended, is ending or failed is retired;
/// one that ended or failed with no open claim or delivery is finished, which frees its capacity.
pub(crate) fn apply(state: &mut State, host: &str, observed: &JvmSessionStatus) {
    let id = &observed.id;
    let empty = !state.claims.values().any(|claim| claim.session == *id && claim.phase != Phase::Released);
    let Some(session) = state.sessions.get_mut(id).filter(|session| session.host == host && !session.finished) else {
        return;
    };
    if !matches(id, session, observed) {
        tracing::debug!(session = id, "ignoring a mismatched session report");
        return;
    }
    match observed.phase() {
        JvmSessionPhase::Ended | JvmSessionPhase::Failed => {
            session.retired = true;
            session.finish_requested = true;
            session.finished = empty && observed.prepared == 0 && observed.attached == 0;
        }
        JvmSessionPhase::Ending => {
            session.retired = true;
            session.finish_requested = true;
        }
        _ => {}
    }
}

/// Whether `observed` reports the session control recorded as `id`.
pub(crate) fn matches(id: &str, session: &SessionState, observed: &JvmSessionStatus) -> bool {
    observed.id == id && observed.session_type == session.session_type && observed.capacity == session.capacity
}
