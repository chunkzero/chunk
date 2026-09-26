//! Recovery from the log: it decides which of a JVM's deliveries remain owned.

use chunk_proto::v1::{DeliveryPhase, PlayerWithdrawal, ProcessInventory, gameplay_client::GameplayClient};

use crate::{
    Control, Result, RuntimeConnection,
    client::{auth, channel},
    state::Phase,
};

impl Control {
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
