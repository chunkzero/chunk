use std::collections::BTreeSet;

use chunk_proto::v1::ShutdownNodeRequest;
use prost::Message;

use crate::{
    Control, Error, Result,
    drain::retire_host,
    state::{Phase, State},
};

impl Control {
    /// Drains hosts that have had no unfinished session or open claim for the idle timeout,
    /// and forgets finished sessions, stopped hosts and its own drains once nothing references them.
    pub(crate) fn retire_idle_hosts(&self) -> Result<()> {
        let now = crate::now_ms();
        let mut authority = self.authority()?;
        // Most passes change nothing, so only commit when the current state would change.
        if self.tidy(&mut authority.read()?, now)? {
            authority.update(|state| self.tidy(state, now))?;
        }
        let retained = authority.read()?.hosts.into_keys().collect();
        // Keep placement excluded until pruning finishes, so a newly allocated host cannot be removed.
        if let Err(error) = self.host.prune(&retained) {
            tracing::warn!(%error, "stopped host cleanup will be retried");
        }
        self.observations
            .lock()
            .map_err(|_| Error::Unresolved("health observations poisoned"))?
            .retain(|id, _| retained.contains(id));
        Ok(())
    }

    fn tidy(&self, state: &mut State, now: u64) -> Result<bool> {
        let open: BTreeSet<_> =
            state.claims.values().filter(|c| c.phase != Phase::Released).map(|c| c.session.clone()).collect();
        let (sessions, drains, hosts) = (state.sessions.len(), state.drains.len(), state.hosts.len());
        state.sessions.retain(|id, session| !session.finished || open.contains(id));
        state.drains.retain(|_, drain| !drain.automatic || !self.host.stopped(&drain.host));
        state.hosts.retain(|id, host| {
            !host.retired
                || !self.host.stopped(id)
                || state.sessions.values().any(|session| session.host == *id)
                || state.drains.values().any(|drain| drain.host == *id)
        });
        let mut changed =
            sessions != state.sessions.len() || drains != state.drains.len() || hosts != state.hosts.len();
        let timeout = u64::from(self.config.idle_node_timeout_seconds) * 1000;
        if timeout == 0 {
            return Ok(changed);
        }
        let busy: BTreeSet<_> = state
            .sessions
            .iter()
            .filter(|(id, session)| !session.finished || open.contains(*id))
            .map(|(_, session)| session.host.clone())
            .collect();
        let mut expired = Vec::new();
        for (id, host) in state.hosts.iter_mut().filter(|(_, host)| !host.retired) {
            let since = (!busy.contains(id)).then(|| host.idle_since_ms.unwrap_or(now));
            changed |= since != host.idle_since_ms;
            host.idle_since_ms = since;
            if since.is_some_and(|since| now.saturating_sub(since) >= timeout) {
                expired.push(id.clone());
            }
        }
        for id in expired {
            let operation = format!("idle/{id}");
            let request =
                ShutdownNodeRequest { operation_id: operation.clone(), host_id: id.clone(), timeout_seconds: 0 };
            retire_host(state, operation, request.encode_to_vec(), 0, true, |_| Ok(id))?;
            changed = true;
        }
        Ok(changed)
    }
}
