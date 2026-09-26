//! Who a request to core may act for, checked against control's current state.

use crate::{Control, Error, Result};

/// A session a JVM's host runs, as app code sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScope {
    pub app: String,
    /// The deployment version the host's release runs.
    pub deployment: String,
}

impl Control {
    /// The host whose running JVM holds `credential`.
    #[must_use]
    pub fn authenticate(&self, credential: &str) -> Option<String> {
        self.host.authenticate(credential)
    }

    /// `session`, which `host` must run. A named `player` must have arrived on it through their current claim.
    /// # Errors
    /// Rejects a session `host` does not run and a player not on it as invalid, and a finished session as stopped.
    pub fn session_scope(&self, host: &str, session: &str, player: Option<&str>) -> Result<SessionScope> {
        let state = self.state()?;
        let running = state.sessions.get(session).filter(|running| running.host == host);
        let running = running.ok_or(Error::Invalid("host does not run this session"))?;
        if running.finished {
            return Err(Error::Stopped);
        }
        if let Some(player) = player {
            let claim = state.players.get(player).and_then(|owner| owner.current.as_ref());
            let claim = claim.and_then(|operation| state.claims.get(operation));
            if !claim.is_some_and(|claim| claim.session == session && claim.phase == crate::state::Phase::Arrived) {
                return Err(Error::Invalid("player has not arrived on this session"));
            }
        }
        let owner = state.hosts.get(host).ok_or(Error::Invalid("unknown host"))?;
        Ok(SessionScope { app: owner.app.clone(), deployment: owner.release.clone() })
    }

    /// Whether `player` owns a current or pending claim.
    /// # Errors
    /// Reports unreadable state.
    pub fn holds_claim(&self, player: &str) -> Result<bool> {
        Ok(self.state()?.players.contains_key(player))
    }
}
