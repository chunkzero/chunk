use chunk_proto::v1::{
    ActivateClaim, Assignment, ClaimIdentity, ClaimPhase, ClaimRequest, DeliveryPhase, PlayerWithdrawal,
    gameplay_client::GameplayClient, process_control_client::ProcessControlClient,
};
use prost::Message;

use crate::{
    Control, Error, Result,
    placement::{auth, channel},
    state::{Claim, Phase, State},
};

impl Control {
    /// Recovers the same delivery from runtime inventory; never replaces its TCP connection.
    /// # Errors
    /// An unavailable runtime leaves ownership unresolved and retained.
    pub async fn inspect(&self, request: ClaimRequest) -> Result<Assignment> {
        let operation = self.operation(&request.operation_id)?;
        let _guard = operation.lock().await;
        let claim = self
            .state()?
            .claims
            .get(&request.operation_id)
            .cloned()
            .ok_or(Error::Invalid("unknown claim"))?;
        claim.matches(&request)?;
        self.reconcile(&request.operation_id).await
    }

    /// Records admission intent; native Minecraft login attaches the prepared delivery.
    /// # Errors
    /// Rejects stale identity and retains authority after ambiguous runtime replies.
    pub async fn activate(&self, request: ActivateClaim) -> Result<Assignment> {
        let identity = request.claim.as_ref().ok_or(Error::Invalid("missing claim identity"))?;
        let operation = self.operation(&identity.operation_id)?;
        let _guard = operation.lock().await;
        self.update(|state| {
            let claim = state
                .claims
                .get_mut(&identity.operation_id)
                .ok_or(Error::Invalid("unknown claim"))?;
            if claim.identity(&identity.operation_id) != *identity
                || claim.assignment.is_none()
                || state.players.get(&claim.player).and_then(|p| p.current.as_ref()) != Some(&identity.operation_id)
                || matches!(claim.phase, Phase::Withdrawing | Phase::Released)
            {
                return Err(Error::Invalid("stale activation"));
            }
            claim.activated = true;
            if claim.phase == Phase::Reserved {
                set_phase(claim, Phase::Activating)?;
            }
            Ok(())
        })?;
        self.reconcile(&identity.operation_id).await
    }

    /// Withdraws only this exact operation and releases capacity after affirmative fencing.
    /// # Errors
    /// Unreachable control channels retain the reservation and membership.
    pub async fn cancel(&self, request: ClaimRequest) -> Result<ClaimIdentity> {
        let operation = self.operation(&request.operation_id)?;
        let _guard = operation.lock().await;
        let state = self.state()?;
        let claim = state
            .claims
            .get(&request.operation_id)
            .ok_or(Error::Invalid("unknown claim"))?;
        claim.matches(&request)?;
        let identity = claim.identity(&request.operation_id);
        if claim.phase == Phase::Released {
            return Ok(identity);
        }
        let host = state
            .sessions
            .get(&claim.session)
            .ok_or(Error::Invalid("missing session"))?
            .host
            .clone();
        self.update(|s| {
            set_phase(
                s.claims
                    .get_mut(&request.operation_id)
                    .ok_or(Error::Invalid("unknown claim"))?,
                Phase::Withdrawing,
            )
        })?;
        if !self.host.stopped(&host) {
            let runtime = self.runtime(&state, &host).await?;
            let withdrawn = GameplayClient::new(channel(&runtime).await?)
                .withdraw_player(auth(
                    &runtime,
                    PlayerWithdrawal {
                        operation_id: request.operation_id.clone(),
                        owner_generation: claim.delivery_generation,
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
        self.update(|state| release(state, &request.operation_id))?;
        Ok(identity)
    }

    async fn reconcile(&self, operation: &str) -> Result<Assignment> {
        let state = self.state()?;
        let claim = state.claims.get(operation).ok_or(Error::Invalid("unknown claim"))?;
        let assignment = claim
            .assignment
            .as_ref()
            .ok_or(Error::Unresolved("claim preparation incomplete"))?;
        if claim.phase == Phase::Released {
            return Ok(Assignment::decode(assignment.as_slice())?);
        }
        let host = &state
            .sessions
            .get(&claim.session)
            .ok_or(Error::Invalid("missing session"))?
            .host;
        if self.host.stopped(host) {
            self.update(|state| release(state, operation))?;
        } else {
            let runtime = self.runtime(&state, host).await?;
            let inventory = ProcessControlClient::new(channel(&runtime).await?)
                .max_decoding_message_size(8 * 1024 * 1024)
                .inventory(auth(&runtime, runtime.identity.clone(), 3)?)
                .await?
                .into_inner();
            if inventory.identity.as_ref() != Some(&runtime.identity) {
                return Err(Error::Invalid("inventory identity mismatch"));
            }
            let binding = inventory
                .deliveries
                .iter()
                .find(|d| d.delivery.as_ref().is_some_and(|d| d.operation_id == operation))
                .ok_or(Error::Unresolved("delivery absent from runtime inventory"))?;
            let delivery = binding.delivery.as_ref().ok_or(Error::Invalid("inventory delivery"))?;
            if delivery.owner_generation != claim.delivery_generation
                || delivery.membership_generation != claim.membership_generation
                || delivery.proxy_id != claim.proxy
                || delivery.session.as_ref().map(|s| &s.id) != Some(&claim.session)
                || delivery.session_generation != 1
                || delivery.runtime_id != runtime.identity.runtime_id
                || delivery.process_generation != runtime.identity.generation
                || delivery.deployment != runtime.identity.deployment
            {
                return Err(Error::Invalid("inventory binding mismatch"));
            }
            let phase = match DeliveryPhase::try_from(binding.phase).ok() {
                Some(DeliveryPhase::Arrived) => Phase::Arrived,
                Some(DeliveryPhase::Attached) => Phase::Attached,
                Some(DeliveryPhase::Closed) => Phase::Released,
                Some(DeliveryPhase::Withdrawing) => Phase::Withdrawing,
                Some(DeliveryPhase::Prepared) => claim.phase,
                _ => return Err(Error::Unresolved("unknown runtime delivery phase")),
            };
            if phase != claim.phase {
                self.update(|state| {
                    if phase == Phase::Released {
                        release(state, operation)
                    } else {
                        set_phase(
                            state.claims.get_mut(operation).ok_or(Error::Invalid("unknown claim"))?,
                            phase,
                        )
                    }
                })?;
            }
        }
        let state = self.state()?;
        let bytes = state
            .claims
            .get(operation)
            .and_then(|c| c.assignment.as_ref())
            .ok_or(Error::Unresolved("assignment missing"))?;
        Ok(Assignment::decode(bytes.as_slice())?)
    }

    /// Stops the owned runtime processes. Dropping control alone preserves them for recovery.
    /// # Errors
    /// Reports unresolved hosts; a failed stop must not be treated as a fencing acknowledgment.
    pub async fn shutdown(&self) -> Result<()> {
        self.draining.store(true, std::sync::atomic::Ordering::Release);
        let state = self.state()?;
        for id in state.hosts.keys() {
            if self.host.stopped(id) {
                continue;
            }
            let runtime = self.runtime(&state, id).await?;
            let _ = ProcessControlClient::new(channel(&runtime).await?)
                .stop_process(auth(&runtime, runtime.identity.clone(), 15)?)
                .await;
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            while !self.host.stopped(id) {
                if tokio::time::Instant::now() >= deadline {
                    return Err(Error::Unresolved("runtime stop not confirmed"));
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
        Ok(())
    }
}

fn release(state: &mut State, operation: &str) -> Result<()> {
    let claim = state.claims.get_mut(operation).ok_or(Error::Invalid("unknown claim"))?;
    set_phase(claim, Phase::Released)?;
    if let Some(player) = state.players.get_mut(&claim.player)
        && player.current.as_deref() == Some(operation)
    {
        player.current = None;
    }
    Ok(())
}

fn set_phase(claim: &mut Claim, phase: Phase) -> Result<()> {
    claim.phase = phase;
    if let Some(bytes) = &mut claim.assignment {
        let mut assignment = Assignment::decode(bytes.as_slice())?;
        assignment.phase = match phase {
            Phase::Reserved => ClaimPhase::Reserved,
            Phase::Activating => ClaimPhase::Activating,
            Phase::Attached => ClaimPhase::Attached,
            Phase::Arrived => ClaimPhase::Arrived,
            Phase::Withdrawing => ClaimPhase::Withdrawing,
            Phase::Released => ClaimPhase::Released,
        } as i32;
        *bytes = assignment.encode_to_vec();
    }
    Ok(())
}
