use std::collections::{BTreeMap, BTreeSet};

use chunk_proto::v1::{ClaimRequest, ShutdownNodeRequest};
use prost::Message;

use crate::{
    Control, Error, Result,
    drain::retire_host,
    releases::Launches,
    state::{Capacity, Phase, State},
};

/// Long enough for callers to retry an operation after its release, short enough to bound history.
const RELEASED_RETENTION_MS: u64 = 300_000;

impl Control {
    /// Drains hosts that have had no unfinished session or open claim for their release's idle timeout, and forgets
    /// finished sessions, released hosts, its own drains, old released claims and releases other than the current
    /// one once nothing references them and every launch is attributed to its host.
    pub(crate) fn retire_idle_hosts(&self) -> Result<()> {
        let now = crate::now_ms();
        let launches = self.launches()?;
        let mut writer = self.authority.writer()?;
        writer.update(|state| Self::tidy(state, now, &launches))?;
        let retained = self.state()?.hosts.keys().cloned().collect();
        // Keep placement excluded until pruning finishes, so a newly allocated host cannot be removed.
        if let Err(error) = self.host.prune(&retained) {
            tracing::warn!(%error, "stopped host cleanup will be retried");
        }
        self.observations
            .lock()
            .map_err(|_| Error::Unresolved("health observations poisoned"))?
            .retain(|id, _| retained.contains(id));
        self.jvms.retain(|id| retained.contains(id) || self.host.connection(id).is_some());
        Ok(())
    }

    /// Forgets unreferenced rows and tracks idle hosts.
    fn tidy(state: &mut State, now: u64, launches: &Launches) -> Result<()> {
        // A released move claim stays while the other end is open: the source's move checks
        // read its destination's outcome, and a destination's activation checks its fenced source.
        let source = |request: &[u8]| ClaimRequest::decode(request).ok().and_then(|request| request.source);
        let open = |operation: &str| state.claims.get(operation).is_some_and(|claim| claim.phase != Phase::Released);
        let mut referenced: BTreeSet<_> = state
            .claims
            .values()
            .filter(|claim| claim.phase != Phase::Released)
            .filter_map(|claim| source(&claim.request).map(|source| source.operation_id))
            .collect();
        referenced.extend(
            state
                .moves
                .iter()
                .filter(|(_, intent)| source(&intent.request).is_some_and(|source| open(&source.operation_id)))
                .map(|(destination, _)| destination.clone()),
        );
        let claims = select(&state.claims, |operation, claim| {
            claim.released_at_ms.is_some_and(|at| now.saturating_sub(at) >= RELEASED_RETENTION_MS)
                && !referenced.contains(operation)
        });
        remove(&mut state.claims, claims);
        let moves = select(&state.moves, |operation, intent| {
            !state.claims.contains_key(operation)
                && source(&intent.request).is_none_or(|source| !state.claims.contains_key(&source.operation_id))
        });
        remove(&mut state.moves, moves);
        let rosters =
            select(&state.rosters, |_, roster| roster.members.iter().all(|member| !state.claims.contains_key(member)));
        remove(&mut state.rosters, rosters);
        let open: BTreeSet<_> =
            state.claims.values().filter(|c| c.phase != Phase::Released).map(|c| c.session.clone()).collect();
        let sessions = select(&state.sessions, |id, session| session.finished && !open.contains(id));
        remove(&mut state.sessions, sessions);
        let drains = select(&state.drains, |_, drain| drain.automatic && state.released(&drain.host));
        remove(&mut state.drains, drains);
        let hosts = select(&state.hosts, |id, host| {
            host.capacity == Capacity::Released
                && !state.sessions.values().any(|session| session.host == *id)
                && !state.drains.values().any(|drain| drain.host == *id)
        });
        remove(&mut state.hosts, hosts);
        let attributed = launches.attributed(state);
        let releases = select(&state.releases, |name, _| {
            attributed
                && state.current.as_deref() != Some(name)
                && !state.hosts.values().any(|host| host.release == name)
        });
        remove(&mut state.releases, releases);
        let busy: BTreeSet<_> = state
            .sessions
            .iter()
            .filter(|(id, session)| !session.finished || open.contains(*id))
            .map(|(_, session)| session.host.clone())
            .collect();
        let mut expired = Vec::new();
        for (id, host) in state.hosts.iter_mut().filter(|(_, host)| !host.retired) {
            let timeout = state
                .releases
                .get(&host.release)
                .map_or(0, |release| u64::from(release.release.idle_node_timeout_seconds) * 1000);
            if timeout == 0 {
                continue;
            }
            let since = (!busy.contains(id)).then(|| host.idle_since_ms.unwrap_or(now));
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
        }
        Ok(())
    }
}

/// The IDs of rows `forget` accepts.
fn select<T>(rows: &BTreeMap<String, T>, mut forget: impl FnMut(&str, &T) -> bool) -> Vec<String> {
    rows.iter().filter(|(id, row)| forget(id, row)).map(|(id, _)| id.clone()).collect()
}

fn remove<T>(rows: &mut BTreeMap<String, T>, ids: Vec<String>) {
    for id in ids {
        rows.remove(&id);
    }
}
