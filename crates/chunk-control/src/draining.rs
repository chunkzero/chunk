//! Releases another one replaced. Such a release places no new sessions, while its existing sessions keep running. A
//! draining one retires once none of its sessions has players, or once its deadline passes. Within the reconnect
//! grace, a player who left one of its sessions logs in to that session again, unless its session type opted out.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use crate::{
    Control, Error, Generation, Result,
    state::{Capacity, Claim, Phase, ReleaseDrain, ReleaseState, State},
};

/// How long after leaving a session of a draining release a player who logs in again returns to it.
pub const RECONNECT_GRACE: Duration = Duration::from_secs(120);

/// How long a draining release stays without players before it retires, so players on their way in arrive first.
const SETTLE_MS: u64 = 10_000;

/// How a release drains once another replaces it. Both limits are durations counted from when it started draining, even
/// when set later; an unset one never passes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrainPolicy {
    /// After this, its sessions take no reconnects and its arrived players move to the current release where they can.
    pub max_age: Option<Duration>,
    /// After this, its hosts stop, disconnecting whoever remains.
    pub deadline: Option<Duration>,
}

impl ReleaseState {
    /// Starts draining at `now` unless it drains already, and keeps the earlier of each limit.
    pub(crate) fn start_draining(&mut self, now: u64, policy: DrainPolicy) {
        self.drain.get_or_insert(ReleaseDrain { since: now, reconnects_until: None, stops_at: None }).shorten(policy);
    }
}

impl ReleaseDrain {
    /// Sets each limit to the earlier of its own and `policy`'s, counted from `since`.
    fn shorten(&mut self, policy: DrainPolicy) {
        let after = |limit: Option<Duration>| {
            limit.map(|limit| self.since.saturating_add(u64::try_from(limit.as_millis()).unwrap_or(u64::MAX)))
        };
        let limits =
            (earliest(self.reconnects_until, after(policy.max_age)), earliest(self.stops_at, after(policy.deadline)));
        (self.reconnects_until, self.stops_at) = limits;
    }
}

impl Control {
    /// Shortens the limits of every draining release to `policy`'s, counted from when it started draining. A limit
    /// the policy would lengthen stays.
    /// # Errors
    /// Reports a stopped store.
    pub fn set_drain_policy(&self, policy: DrainPolicy) -> Result<()> {
        let shortened = |drain: &ReleaseDrain| {
            let mut shortened = drain.clone();
            shortened.shorten(policy);
            shortened != *drain
        };
        let state = self.state()?;
        if !state.releases.values().filter_map(|release| release.drain.as_ref()).any(shortened) {
            return Ok(());
        }
        self.update(|state| {
            for drain in state.releases.values_mut().filter_map(|release| release.drain.as_mut()) {
                drain.shorten(policy);
            }
            Ok(())
        })
    }

    /// Drains `deployment`'s release under `policy`, whose limits count from when it started draining, keeping the
    /// earlier of each limit when it already drains. Returns whether it has retired and every one of its hosts has
    /// stopped, as for an unknown release.
    /// # Errors
    /// Rejects the current release and reports a stopped store.
    pub fn drain_release(&self, deployment: &str, policy: DrainPolicy) -> Result<bool> {
        let now = crate::now_ms();
        self.update(|state| {
            if state.current.as_deref() == Some(deployment) {
                return Err(Error::Invalid("the current release cannot retire"));
            }
            let Some(release) = state.releases.get_mut(deployment).filter(|release| !release.retired) else {
                return Ok(());
            };
            release.start_draining(now, policy);
            Ok(())
        })?;
        self.release_stopped(deployment)
    }

    /// The draining releases that are due: those past their deadline, and those with no players, no player who may
    /// still return, and nothing joining.
    /// # Errors
    /// Reports a stopped store.
    pub fn due_releases(&self) -> Result<Vec<String>> {
        let state = self.state()?;
        Ok(Self::releases_due(&state).0)
    }

    /// Moves the arrived players of releases past their maximum age to the current release, and retires the releases
    /// that are due unless the caller does.
    pub(crate) fn progress_releases(&self) -> Result<()> {
        let state = self.state()?;
        let (due, moving) = Self::releases_due(&state);
        for (operation, claim) in moving {
            if let Err(error) = self.move_player(crate::moves::evacuation(operation, claim)?) {
                tracing::debug!(%error, player = claim.player, "draining release keeps its player");
            }
        }
        if self.config.defers_retirement {
            return Ok(());
        }
        for name in due {
            if let Err(error) = self.retire_release(&name) {
                tracing::warn!(%error, deployment = name, "drained release not retired");
            }
        }
        Ok(())
    }

    /// The releases of `state` that are due, and the claims of those past their maximum age that should move.
    fn releases_due(state: &State) -> (Vec<String>, Vec<(&String, &Claim)>) {
        let now = crate::now_ms();
        let latest = latest_claims(state);
        let (mut due, mut moving) = (Vec::new(), Vec::new());
        for (name, drain) in
            state.releases.iter().filter(|(_, release)| !release.retired).filter_map(|(name, release)| {
                (state.current.as_ref() != Some(name)).then_some((name, release.drain.as_ref()?))
            })
        {
            let claims: Vec<_> = state.claims.iter().filter(|(_, claim)| on_release(state, claim, name)).collect();
            let kept = |(operation, claim): &(&String, &Claim)| {
                claim.phase != Phase::Released
                    || claim.released_at_ms.is_some_and(|at| now.saturating_sub(at) < SETTLE_MS)
                    || returns(state, &latest, operation, now).is_some()
            };
            if drain.stops_at.is_some_and(|at| now >= at) || !claims.iter().any(kept) {
                due.push(name.clone());
            } else if drain.reconnects_until.is_some_and(|at| now >= at) {
                moving.extend(claims.into_iter().filter(|(_, claim)| claim.phase == Phase::Arrived));
            }
        }
        (due, moving)
    }
}

/// The session of a draining release that `player`, logging in, returns to: the one their latest activated claim left within
/// the reconnect grace, while it still runs, takes reconnects and has room.
pub(crate) fn rejoin(state: &State, player: &str, unavailable: &BTreeSet<String>) -> Option<String> {
    let (operation, _) = state
        .claims
        .iter()
        .filter(|(_, claim)| claim.player == player && claim.activated)
        .max_by_key(|(_, claim)| claim.generation)?;
    let latest = BTreeMap::from([(player, operation.as_str())]);
    let session = returns(state, &latest, operation, crate::now_ms())?;
    let room = state.claims.values().filter(|claim| claim.session == session && claim.phase != Phase::Released).count()
        < state.sessions[session].capacity as usize;
    (room && !unavailable.contains(&state.sessions[session].host)).then(|| session.to_owned())
}

/// The sessions of draining releases that a player may still return to, which must not finish.
pub(crate) fn reconnectable(state: &State, now: u64) -> BTreeSet<String> {
    let latest = latest_claims(state);
    state.claims.keys().filter_map(|operation| returns(state, &latest, operation, now)).map(str::to_owned).collect()
}

/// The session a player may still return to from the claim `operation`: their latest activated claim, released within the
/// reconnect grace from a running session of a draining release, before its maximum age, whose session type takes
/// reconnects.
fn returns<'a>(state: &'a State, latest: &BTreeMap<&str, &str>, operation: &str, now: u64) -> Option<&'a str> {
    let claim = state.claims.get(operation)?;
    let grace = u64::try_from(RECONNECT_GRACE.as_millis()).unwrap_or(u64::MAX);
    if claim.phase != Phase::Released
        || latest.get(claim.player.as_str()) != Some(&operation)
        || claim.released_at_ms.is_none_or(|at| now.saturating_sub(at) >= grace)
    {
        return None;
    }
    let session = state.sessions.get(&claim.session).filter(|session| !session.finished && !session.retired)?;
    let host = state.hosts.get(&session.host).filter(|host| !host.retired && host.capacity == Capacity::Ready)?;
    let release = state.releases.get(&host.release).filter(|release| !release.retired)?;
    let drain = release.drain.as_ref().filter(|_| state.current.as_ref() != Some(&host.release))?;
    let (app, implementation) = session.session_type.split_once('/')?;
    let reconnect = release.release.apps.get(app)?.sessions.get(implementation)?.reconnect;
    (reconnect && drain.reconnects_until.is_none_or(|at| now < at)).then_some(claim.session.as_str())
}

/// Each player's latest activated claim, by generation.
fn latest_claims(state: &State) -> BTreeMap<&str, &str> {
    let mut latest: BTreeMap<&str, (Generation, &str)> = BTreeMap::new();
    for (operation, claim) in state.claims.iter().filter(|(_, claim)| claim.activated) {
        let entry = latest.entry(claim.player.as_str()).or_insert((claim.generation, operation));
        if claim.generation > entry.0 {
            *entry = (claim.generation, operation);
        }
    }
    latest.into_iter().map(|(player, (_, operation))| (player, operation)).collect()
}

fn on_release(state: &State, claim: &Claim, name: &str) -> bool {
    state
        .sessions
        .get(&claim.session)
        .and_then(|session| state.hosts.get(&session.host))
        .is_some_and(|host| host.release == name)
}

fn earliest(current: Option<u64>, next: Option<u64>) -> Option<u64> {
    match (current, next) {
        (Some(current), Some(next)) => Some(current.min(next)),
        (current, next) => current.or(next),
    }
}
