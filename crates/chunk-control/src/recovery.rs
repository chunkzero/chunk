//! Recovery after control restarts: JVMs that outlived it re-attach, and the log decides which of their deliveries
//! remain owned. Until every surviving JVM's deliveries are fenced or its host confirms it stopped, new claims are
//! refused as busy, so a player a JVM still serves without a claim in the log cannot gain a second owner. No timeout
//! reopens admission: an unreachable JVM may still be serving players.

use std::{
    collections::BTreeSet,
    sync::Mutex,
    time::{Duration, Instant},
};

use chunk_proto::v1::{
    DeliveryPhase, PlayerDelivery, PlayerWithdrawal, ProcessIdentity, ProcessInventory, ProcessRegistration,
    ShutdownNodeRequest, gameplay_client::GameplayClient, process_control_client::ProcessControlClient,
};
use prost::Message;
use tokio::sync::Mutex as AsyncMutex;

use crate::{
    Control, Error, Result, RuntimeConnection,
    client::{auth, channel},
    drain::retire_host,
    state::{Claim, Generation, HostState, Phase},
};

/// How often admission warns that it still waits for surviving JVMs. JVMs repeat registration every second.
const RECOVERY_WARNING: Duration = Duration::from_secs(30);

/// Hosts whose surviving deliveries are not yet fenced.
pub(crate) struct Recovery {
    pending: Mutex<Pending>,
    resolving: AsyncMutex<()>,
}

struct Pending {
    hosts: BTreeSet<String>,
    /// When admission next warns that hosts are still pending.
    deadline: Instant,
}

impl Recovery {
    pub fn new(hosts: BTreeSet<String>) -> Self {
        let pending = Pending { hosts, deadline: Instant::now() + RECOVERY_WARNING };
        Self { pending: Mutex::new(pending), resolving: AsyncMutex::new(()) }
    }

    #[cfg(test)]
    pub fn pass_deadline(&self) {
        self.pending.lock().unwrap().deadline = Instant::now();
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Pending>> {
        self.pending.lock().map_err(|_| Error::Unresolved("recovery poisoned"))
    }

    pub fn open(&self) -> Result<bool> {
        Ok(self.lock()?.hosts.is_empty())
    }

    fn reopen(&self, host: &str) -> Result<()> {
        self.lock()?.hosts.insert(host.into());
        Ok(())
    }
}

impl Control {
    /// Admits new claims once recovery has resolved, first trying to resolve it.
    /// # Errors
    /// Reports `Busy` while a surviving JVM's deliveries remain unfenced.
    pub(crate) async fn admit(&self) -> Result<()> {
        self.resolve_recovery().await?;
        if self.recovery.open()? { Ok(()) } else { Err(Error::Busy) }
    }

    /// Fences each pending host's surviving deliveries. Returns at once while another caller is resolving.
    pub(crate) async fn resolve_recovery(&self) -> Result<()> {
        if self.recovery.open()? {
            return Ok(());
        }
        let Ok(_resolving) = self.recovery.resolving.try_lock() else {
            return Ok(());
        };
        let hosts = self.recovery.lock()?.hosts.clone();
        for id in hosts {
            let resolved = match self.recover_host(&id).await {
                Ok(resolved) => resolved,
                Err(error) => {
                    tracing::debug!(%error, host = id, "surviving deliveries remain unfenced");
                    false
                }
            };
            if resolved {
                self.recovery.lock()?.hosts.remove(&id);
            }
        }
        let mut pending = self.recovery.lock()?;
        if !pending.hosts.is_empty() && Instant::now() >= pending.deadline {
            pending.deadline = Instant::now() + RECOVERY_WARNING;
            tracing::warn!(
                hosts = ?pending.hosts,
                "new claims wait until surviving JVMs re-attach or their hosts confirm they stopped"
            );
        }
        Ok(())
    }

    async fn recover_host(&self, id: &str) -> Result<bool> {
        if self.host.stopped(id) {
            return Ok(true);
        }
        let Some(runtime) = self.host.connection(id) else {
            // Without an unowned launch, no JVM from before the restart can run on this host.
            return Ok(!self.host.unresolved(id));
        };
        if let Some(host) = self.state()?.hosts.get(id)
            && !self.runs_host(&runtime, host)
        {
            return Err(Error::Invalid("recovered runtime mismatch"));
        }
        let inventory = ProcessControlClient::new(channel(&runtime).await?)
            .max_decoding_message_size(8 * 1024 * 1024)
            .inventory(auth(&runtime, runtime.identity.clone(), 3)?)
            .await?
            .into_inner();
        if inventory.identity.as_ref() != Some(&runtime.identity) {
            return Err(Error::Invalid("recovered inventory mismatch"));
        }
        self.retire_unknown_operations(&inventory)?;
        let fenced = self.fence_deliveries(&runtime, &inventory).await?;
        if fenced {
            self.retire_orphan(id, &runtime.identity)?;
        }
        Ok(fenced)
    }

    /// Records a fenced JVM whose host row a restore lost as a retiring host, so the host lifecycle stops it: its
    /// drain terminates it at once, and control shutdown stops it like any logged host.
    fn retire_orphan(&self, id: &str, identity: &ProcessIdentity) -> Result<()> {
        self.update(|state| {
            if state.hosts.contains_key(id) {
                return Ok(());
            }
            let host = HostState {
                app: identity.app_id.clone(),
                profile: identity.machine_profile.clone(),
                retired: false,
                idle_since_ms: None,
            };
            state.hosts.insert(id.into(), host);
            let operation = format!("orphan/{id}");
            let request =
                ShutdownNodeRequest { operation_id: operation.clone(), host_id: id.into(), timeout_seconds: 0 };
            retire_host(state, operation, request.encode_to_vec(), 0, true, |_| Ok(id.into()))
        })
    }

    /// Accepts a JVM registration. A JVM launched before control restarted re-attaches only if its host adopts it,
    /// which requires the credential recorded before launch, and it still runs its logged host's app. A JVM whose
    /// host row a restore lost re-attaches by its launch record alone; the log owns none of its deliveries, so
    /// recovery withdraws them all.
    pub(crate) fn register(&self, token: &str, registration: ProcessRegistration) -> Result<ProcessIdentity> {
        let error = match self.host.register(token, registration.clone()) {
            Ok(identity) => return Ok(identity),
            Err(error) => error,
        };
        let (Some(identity), Some(secret)) = (registration.identity.clone(), token.strip_prefix("Bearer ")) else {
            return Err(error);
        };
        if let Some(host) = self.state()?.hosts.get(&identity.runtime_id) {
            let connection = RuntimeConnection {
                endpoint: registration.control_endpoint.clone(),
                token: secret.into(),
                identity: identity.clone(),
                player_endpoint: registration.player_endpoint.clone(),
            };
            if !self.runs_host(&connection, host) {
                return Err(error);
            }
        }
        self.host.adopt(secret, registration)?;
        self.recovery.reopen(&identity.runtime_id)?;
        tracing::info!(host = identity.runtime_id, "re-attached a JVM that outlived control");
        Ok(identity)
    }

    /// Records a released tombstone for each delivery whose operation the log does not know, such as deliveries
    /// prepared by commits a restore lost. A retry of that operation is then rejected instead of reserving the
    /// player again under an operation ID the JVM already holds.
    fn retire_unknown_operations(&self, inventory: &ProcessInventory) -> Result<()> {
        self.update(|state| {
            for delivery in inventory.deliveries.iter().filter_map(|binding| binding.delivery.as_ref()) {
                state.claims.entry(delivery.operation_id.clone()).or_insert_with(|| tombstone(delivery));
            }
            Ok(())
        })
    }

    /// Withdraws open deliveries that no open claim in the log owns with the same generations, using the generation
    /// the JVM holds. `inventory` must be read before state, so every delivery control prepared already has its
    /// claim. Reports whether every such delivery is now withdrawn.
    pub(crate) async fn fence_deliveries(
        &self,
        runtime: &RuntimeConnection,
        inventory: &ProcessInventory,
    ) -> Result<bool> {
        let state = self.state()?;
        let mut gameplay = None;
        let mut fenced = true;
        for binding in inventory.deliveries.iter().filter(|binding| binding.phase != DeliveryPhase::Closed as i32) {
            let Some(delivery) = &binding.delivery else {
                continue;
            };
            let owned = state.claims.get(&delivery.operation_id).is_some_and(|claim| {
                claim.phase != Phase::Released
                    && claim.generation.wire() == delivery.owner_generation
                    && claim.membership.wire() == delivery.membership_generation
            });
            if owned {
                continue;
            }
            let client = match &mut gameplay {
                Some(client) => client,
                None => gameplay.insert(GameplayClient::new(channel(runtime).await?)),
            };
            let withdrawal = PlayerWithdrawal {
                operation_id: delivery.operation_id.clone(),
                owner_generation: delivery.owner_generation,
            };
            if let Err(error) = client.withdraw_player(auth(runtime, withdrawal, 10)?).await {
                tracing::debug!(%error, operation = delivery.operation_id, "unowned delivery withdrawal will be retried");
                fenced = false;
            }
        }
        Ok(fenced)
    }
}

/// A released claim standing for `delivery`'s operation. Its empty request matches no retry.
fn tombstone(delivery: &PlayerDelivery) -> Claim {
    let now = crate::now_ms();
    Claim {
        request: Vec::new(),
        player: delivery.player.as_ref().map(|player| player.id.clone()).unwrap_or_default(),
        proxy: delivery.proxy_id.clone(),
        membership: Generation::from_wire(delivery.membership_generation),
        generation: Generation::from_wire(delivery.owner_generation),
        session: delivery.session.as_ref().map(|session| session.id.clone()).unwrap_or_default(),
        phase: Phase::Released,
        assignment: None,
        activated: false,
        created_at_ms: now,
        released_at_ms: Some(now),
        roster: None,
    }
}
