//! Who a request to core may act for, checked against control's current state.

use chunk_proto::control::v1::{ClaimIdentity, ClaimRequest};
use prost::Message;

use crate::{Control, Error, Result, state::Phase};

/// A session a JVM's host runs, as app code sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionScope {
    pub app: String,
    /// The deployment version the host's release runs.
    pub deployment: String,
}

/// A player's current claim, arrived through a gateway, and where it's delivered.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrivedClaim {
    pub request: ClaimRequest,
    pub identity: ClaimIdentity,
    pub session: String,
    pub session_type: String,
    /// The session's app and the deployment its host runs.
    pub scope: SessionScope,
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

    /// The host whose JVM, launched before control restarted, holds `credential` and has yet to re-register.
    #[must_use]
    pub fn unadopted(&self, credential: &str) -> Option<String> {
        self.host.unadopted(credential)
    }

    /// `session`, which `host` must run. A named `player` must be delivered to it through their current claim, in any
    /// phase from its reservation until the delivery closes or the claim is released.
    /// # Errors
    /// Rejects a session `host` does not run and a player not held on it as invalid, and a finished session as stopped.
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
            if !claim.is_some_and(|claim| claim.session == session && claim.phase != Phase::Released) {
                return Err(Error::Invalid("player is not delivered to this session"));
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

    /// `player`'s current claim, which must be held through `gateway` and have arrived.
    /// # Errors
    /// Rejects a player without such a claim as invalid.
    pub fn arrived_claim(&self, gateway: &str, player: &str) -> Result<ArrivedClaim> {
        let state = self.state()?;
        let operation = state.players.get(player).and_then(|owner| owner.current.as_ref());
        let claim = operation.and_then(|operation| state.claims.get(operation).map(|claim| (operation, claim)));
        let arrived = claim.filter(|(_, claim)| claim.proxy == gateway && claim.phase == Phase::Arrived);
        let (operation, claim) = arrived.ok_or(Error::Invalid("the gateway holds no arrived claim for this player"))?;
        if claim.request.is_empty() {
            return Err(Error::Invalid("claim operation lost in a restore"));
        }
        let session = state.sessions.get(&claim.session).ok_or(Error::Invalid("missing claim session"))?;
        let host = state.hosts.get(&session.host).ok_or(Error::Invalid("unknown host"))?;
        Ok(ArrivedClaim {
            request: ClaimRequest::decode(claim.request.as_slice())?,
            identity: claim.identity(operation),
            session: claim.session.clone(),
            session_type: session.session_type.clone(),
            scope: SessionScope { app: host.app.clone(), deployment: host.release.clone() },
        })
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
