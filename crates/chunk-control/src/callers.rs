//! Who a request to core may act for, checked against control's current state.

use chunk_proto::v1::{ClaimIdentity, ClaimRequest};
use prost::Message;

use crate::{Control, Error, Result};

/// A session a JVM's host runs, as app code sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScope {
    pub app: String,
    /// The deployment version the host's release runs.
    pub deployment: String,
}

/// A claim, or a move control queued, as stored under its operation ID.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredClaim {
    /// The request the claim is made with; a queued move's is the destination claim control prepared for its proxy.
    pub request: ClaimRequest,
    /// The claim's identity, once the operation has claimed.
    pub identity: Option<ClaimIdentity>,
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

    /// Whether `player`'s current or pending claim is held through `gateway`.
    /// # Errors
    /// Reports unreadable state.
    pub fn holds_claim(&self, gateway: &str, player: &str) -> Result<bool> {
        let state = self.state()?;
        let Some(owner) = state.players.get(player) else { return Ok(false) };
        let held = [&owner.current, &owner.pending].into_iter().flatten();
        Ok(held.filter_map(|operation| state.claims.get(operation)).any(|claim| claim.proxy == gateway))
    }

    /// The claim or queued move stored under `operation`, if any.
    /// # Errors
    /// Reports unreadable state, and a claim whose request a restore lost.
    pub fn stored_claim(&self, operation: &str) -> Result<Option<StoredClaim>> {
        let state = self.state()?;
        if let Some(claim) = state.claims.get(operation) {
            if claim.request.is_empty() {
                return Err(Error::Invalid("claim operation lost in a restore"));
            }
            let request = ClaimRequest::decode(claim.request.as_slice())?;
            return Ok(Some(StoredClaim { request, identity: Some(claim.identity(operation)) }));
        }
        let Some(intent) = state.moves.get(operation) else { return Ok(None) };
        Ok(Some(StoredClaim { request: ClaimRequest::decode(intent.request.as_slice())?, identity: None }))
    }
}
