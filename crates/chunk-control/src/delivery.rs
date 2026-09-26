use chunk_proto::v1::{
    ActivateClaim, Assignment, ClaimIdentity, ClaimPhase, ClaimRequest, DeliveryInventory, DeliveryPhase,
    PlayerDelivery, PlayerWithdrawal, ProcessIdentity, gameplay_client::GameplayClient,
};
use prost::Message;

use crate::{
    Control, Error, Result,
    client::{auth, channel},
    state::{Claim, Phase, State},
};

impl Control {
    /// Records admission intent; native Minecraft login attaches the prepared delivery.
    /// # Errors
    /// Rejects stale identity and retains authority after ambiguous runtime replies. Reports `Busy` until recovery
    /// has fenced surviving JVMs.
    pub async fn activate(&self, request: ActivateClaim) -> Result<Assignment> {
        let identity = request.claim.as_ref().ok_or(Error::Invalid("missing claim identity"))?;
        self.admit().await?;
        let operation = self.operation(&identity.operation_id)?;
        let _guard = operation.lock().await;
        let admitted = self.update(|state| {
            crate::moves::authorize_destination(state, identity)?;
            let claim = state.claims.get(&identity.operation_id).ok_or(Error::Invalid("unknown claim"))?;
            if claim.phase == Phase::Reserved && state.sessions[&claim.session].retired {
                return Err(Error::Invalid("destination draining"));
            }
            if claim.identity(&identity.operation_id) != *identity
                || claim.assignment.is_none()
                || state.players.get(&claim.player).and_then(|p| p.current.as_ref()) != Some(&identity.operation_id)
                || matches!(claim.phase, Phase::Withdrawing | Phase::Released)
            {
                return Err(Error::Invalid("stale activation"));
            }
            let members = match claim.roster.clone() {
                Some(roster) => crate::roster::ready(state, &roster, &identity.operation_id)?,
                None => vec![identity.operation_id.clone()],
            };
            for member in &members {
                let claim = state.claims.get_mut(member).ok_or(Error::Invalid("unknown claim"))?;
                claim.activated = true;
                if claim.phase == Phase::Reserved {
                    set_phase(claim, Phase::Activating)?;
                }
            }
            Ok(!members.is_empty())
        })?;
        if !admitted {
            return Err(Error::Unresolved("roster awaiting members"));
        }
        self.apply_reported(&identity.operation_id)?;
        self.reconcile(&identity.operation_id)
    }

    /// Withdraws only this exact operation and releases capacity after affirmative fencing.
    /// # Errors
    /// Unreachable control channels retain the reservation and membership.
    pub async fn cancel(&self, request: ClaimRequest) -> Result<ClaimIdentity> {
        self.cancel_with_failure(request, None).await
    }

    pub(crate) async fn cancel_with_failure(
        &self,
        request: ClaimRequest,
        failure: Option<String>,
    ) -> Result<ClaimIdentity> {
        let operation = self.operation(&request.operation_id)?;
        let _guard = operation.lock().await;
        let abandoned = failure.is_some();
        if self.cancel_intent(&request, failure)? {
            return Ok(ClaimIdentity {
                operation_id: request.operation_id,
                proxy_id: request.proxy_id,
                ..Default::default()
            });
        }
        let state = self.state()?;
        let claim = state.claims.get(&request.operation_id).ok_or(Error::Invalid("unknown claim"))?;
        claim.matches(&request)?;
        let identity = claim.identity(&request.operation_id);
        if claim.phase == Phase::Released {
            return Ok(identity);
        }
        let host = state.sessions.get(&claim.session).ok_or(Error::Invalid("missing session"))?.host.clone();
        // The claim may have been released since it was read, such as with its host's capacity.
        let released = self.update(|s| {
            let claim = s.claims.get_mut(&request.operation_id).ok_or(Error::Invalid("unknown claim"))?;
            if claim.phase == Phase::Released {
                return Ok(true);
            }
            set_phase(claim, Phase::Withdrawing).map(|()| false)
        })?;
        if released {
            return Ok(identity);
        }
        // The recorded failure fences activation; reconciliation owns the remaining withdrawal.
        if abandoned {
            return Ok(identity);
        }
        if !state.released(&host) {
            let runtime = self.host.connection(&host).ok_or(Error::Unresolved("runtime unavailable"))?;
            let withdrawn = GameplayClient::new(channel(&runtime).await?)
                .withdraw_player(auth(
                    &runtime,
                    PlayerWithdrawal {
                        operation_id: request.operation_id.clone(),
                        owner_generation: claim.generation.wire(),
                    },
                    10,
                )?)
                .await;
            match withdrawn {
                Ok(_) => {}
                // A late preparation cannot activate after this operation is canceled.
                Err(error) if error.code() == tonic::Code::NotFound && claim.assignment.is_none() => {}
                Err(error) => return Err(error.into()),
            }
        }
        self.update(|state| {
            if state.claims.get(&request.operation_id).is_some_and(|claim| claim.phase == Phase::Released) {
                return Ok(());
            }
            release(state, &request.operation_id)
        })?;
        Ok(identity)
    }

    /// Withdraws the captured connection and confirms its logical membership has ended.
    /// # Errors
    /// An unresolved withdrawal never authorizes a disconnect notification.
    pub async fn reconcile_departure(&self, request: ClaimRequest) -> Result<chunk_proto::v1::DepartureStatus> {
        let player = request.identity.as_ref().ok_or(Error::Invalid("missing player identity"))?.uuid.clone();
        let identity = self.cancel(request).await?;
        // A player row exists only while it owns a claim, whether in this membership or a newer one.
        let departed = identity.membership_generation != 0 && !self.state()?.players.contains_key(&player);
        Ok(chunk_proto::v1::DepartureStatus { claim: Some(identity), departed })
    }

    /// The claim's current assignment. Releasing its host's capacity released it.
    fn reconcile(&self, operation: &str) -> Result<Assignment> {
        let state = self.state()?;
        let bytes = state
            .claims
            .get(operation)
            .ok_or(Error::Invalid("unknown claim"))?
            .assignment
            .as_ref()
            .ok_or(Error::Unresolved("claim preparation incomplete"))?;
        Ok(Assignment::decode(bytes.as_slice())?)
    }

    /// Stops committing and releases the deployment's scope, so another authority can open it while tasks of this one
    /// still finish.
    pub(crate) fn close(&self) -> Result<()> {
        self.authority.close()
    }

    /// Stops the owned runtime processes directly, without recording it, since the environment store may have
    /// stopped. Dropping control alone preserves them for recovery.
    /// # Errors
    /// Reports unresolved hosts; a failed stop must not be treated as a fencing acknowledgment.
    pub async fn shutdown(&self) -> Result<()> {
        self.draining.store(true, std::sync::atomic::Ordering::Release);
        let state = self.state()?;
        let mut result = Ok(());
        for id in state.hosts.keys().filter(|id| !state.released(id)) {
            match self.host.release(id).await {
                Ok(true) => {}
                Ok(false) => result = Err(Error::Unresolved("JVM shutdown not confirmed")),
                Err(error) => result = Err(error),
            }
        }
        result
    }
}

/// Records the phase a JVM reported for one delivery. A report for a claim control has not prepared yet, or that does
/// not match the claim's generations, session and process, is ignored.
pub(crate) fn apply(
    state: &mut State,
    host: &str,
    identity: &ProcessIdentity,
    binding: &DeliveryInventory,
) -> Result<()> {
    let Some(delivery) = &binding.delivery else {
        return Ok(());
    };
    let operation = &delivery.operation_id;
    let Some(claim) = state.claims.get(operation) else {
        return Ok(());
    };
    if claim.phase == Phase::Released || claim.assignment.is_none() {
        return Ok(());
    }
    if !owns(state, claim, host, identity, delivery) {
        tracing::debug!(operation, "ignoring a stale delivery report");
        return Ok(());
    }
    let phase = match DeliveryPhase::try_from(binding.phase) {
        Ok(DeliveryPhase::Arrived) => Phase::Arrived,
        Ok(DeliveryPhase::Attached) => Phase::Attached,
        Ok(DeliveryPhase::Closed) => Phase::Released,
        Ok(DeliveryPhase::Withdrawing) => Phase::Withdrawing,
        _ => return Ok(()),
    };
    // A delivery only moves forward; an older report of the same generation cannot undo a newer phase.
    if phase <= claim.phase {
        return Ok(());
    }
    if phase == Phase::Released {
        release(state, operation)
    } else {
        set_phase(state.claims.get_mut(operation).ok_or(Error::Invalid("unknown claim"))?, phase)
    }
}

/// Whether `delivery` is the one control prepared for `claim` on `host`'s current process.
fn owns(state: &State, claim: &Claim, host: &str, identity: &ProcessIdentity, delivery: &PlayerDelivery) -> bool {
    delivery.owner_generation == claim.generation.wire()
        && delivery.membership_generation == claim.membership.wire()
        && delivery.proxy_id == claim.proxy
        && delivery.session.as_ref().map(|s| &s.id) == Some(&claim.session)
        && delivery.session_generation == 1
        && delivery.runtime_id == identity.runtime_id
        && delivery.process_generation == identity.generation
        && delivery.deployment == identity.deployment
        && state.sessions.get(&claim.session).is_some_and(|session| session.host == host)
}

pub(crate) fn release(state: &mut State, operation: &str) -> Result<()> {
    let claim = state.claims.get_mut(operation).ok_or(Error::Invalid("unknown claim"))?;
    set_phase(claim, Phase::Released)?;
    claim.released_at_ms.get_or_insert(crate::now_ms());
    let session_id = claim.session.clone();
    let (player, roster) = (claim.player.clone(), claim.roster.clone());
    state.disown(&player, operation);
    if let Some(roster) = roster {
        crate::roster::fail(state, &roster, "roster member left");
    }
    if !state.claims.values().any(|claim| claim.session == session_id && claim.phase != Phase::Released)
        && let Some(session) = state.sessions.get_mut(&session_id)
    {
        session.empty_since_ms.get_or_insert(crate::now_ms());
    }
    Ok(())
}

fn set_phase(claim: &mut Claim, phase: Phase) -> Result<()> {
    claim.phase = phase;
    if let Some(bytes) = &mut claim.assignment {
        let mut assignment = Assignment::decode(bytes.as_slice())?;
        assignment.phase = ClaimPhase::from(phase).into();
        *bytes = assignment.encode_to_vec();
    }
    Ok(())
}
