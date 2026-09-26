//! Recovery after control restarts: JVMs that outlived it re-attach, and the log decides which of their deliveries
//! remain owned.

use chunk_proto::v1::{
    DeliveryPhase, PlayerWithdrawal, ProcessIdentity, ProcessInventory, ProcessRegistration,
    gameplay_client::GameplayClient,
};
use sha2::{Digest, Sha256};

use crate::{
    Control, Result, RuntimeConnection,
    client::{auth, channel},
    state::{Phase, ProcessRecord},
};

impl Control {
    /// Accepts a JVM registration. A JVM launched before control restarted re-attaches only by presenting the
    /// credential whose digest the log recorded for its host, with the exact logged process identity.
    pub(crate) fn register(&self, token: &str, registration: ProcessRegistration) -> Result<ProcessIdentity> {
        let error = match self.host.register(token, registration.clone()) {
            Ok(identity) => return Ok(identity),
            Err(error) => error,
        };
        let (Some(identity), Some(secret)) = (registration.identity.clone(), token.strip_prefix("Bearer ")) else {
            return Err(error);
        };
        let state = self.state()?;
        let Some(host) = state.hosts.get(&identity.runtime_id) else {
            return Err(error);
        };
        let connection = RuntimeConnection {
            endpoint: registration.control_endpoint.clone(),
            token: secret.into(),
            identity: identity.clone(),
            player_endpoint: registration.player_endpoint.clone(),
        };
        if host.process.as_ref() != Some(&record(&connection)) || !self.runs_host(&connection, host) {
            return Err(error);
        }
        self.host.adopt(secret, registration)?;
        tracing::info!(host = identity.runtime_id, "re-attached a JVM that outlived control");
        Ok(identity)
    }

    /// Logs which process serves `id`, so it can re-attach after a restart.
    pub(crate) fn record_process(&self, id: &str, runtime: &RuntimeConnection) -> Result<()> {
        let process = record(runtime);
        if self.state()?.hosts.get(id).is_some_and(|host| host.process.as_ref() == Some(&process)) {
            return Ok(());
        }
        self.update(|state| {
            if let Some(host) = state.hosts.get_mut(id) {
                host.process = Some(process);
            }
            Ok(())
        })
    }

    /// Withdraws open deliveries that no open claim in the log owns with the same generations, such as deliveries
    /// prepared by commits a restore lost. `inventory` must be read before state, so every delivery control prepared
    /// already has its claim.
    pub(crate) async fn fence_deliveries(
        &self,
        runtime: &RuntimeConnection,
        inventory: &ProcessInventory,
    ) -> Result<()> {
        let state = self.state()?;
        let mut gameplay = None;
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
            }
        }
        Ok(())
    }
}

fn record(runtime: &RuntimeConnection) -> ProcessRecord {
    ProcessRecord {
        id: runtime.identity.process_id.clone(),
        generation: runtime.identity.generation,
        token_sha256: format!("{:x}", Sha256::digest(runtime.token.as_bytes())),
    }
}
