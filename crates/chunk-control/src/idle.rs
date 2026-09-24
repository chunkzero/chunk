use std::collections::BTreeSet;

use chunk_proto::v1::ShutdownNodeRequest;
use prost::Message;

use crate::{
    Control, Result,
    drain::retire_host,
    state::{Phase, State},
};

/// How long a stopped host's drain stays answerable to repeated drain requests.
const DRAIN_RETENTION_MS: u64 = 10 * 60 * 1000;

impl Control {
    /// Drains hosts that have had no unfinished session or open claim for the idle timeout,
    /// and forgets finished sessions and old drains that nothing references anymore.
    pub(crate) fn retire_idle_hosts(&self) -> Result<()> {
        let now = crate::now_ms();
        // Most passes change nothing, so only commit when the current state would change.
        if self.tidy(&mut self.state()?, now)? {
            self.update(|state| self.tidy(state, now))?;
        }
        Ok(())
    }

    fn tidy(&self, state: &mut State, now: u64) -> Result<bool> {
        let open: BTreeSet<_> =
            state.claims.values().filter(|c| c.phase != Phase::Released).map(|c| c.session.clone()).collect();
        let (sessions, drains) = (state.sessions.len(), state.drains.len());
        state.sessions.retain(|id, session| !session.finished || open.contains(id));
        state.drains.retain(|_, drain| {
            !self.host.stopped(&drain.host) || now < drain.deadline_ms.saturating_add(DRAIN_RETENTION_MS)
        });
        let mut changed = sessions != state.sessions.len() || drains != state.drains.len();
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
            retire_host(state, operation, request.encode_to_vec(), 0, |_| Ok(id))?;
            changed = true;
        }
        Ok(changed)
    }
}
