use std::sync::Arc;

use prost::Message;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{Control, Result, state::Phase};

impl Control {
    /// Expires unactivated reservations, withdraws claims whose capacity is retired or released, and repairs what JVMs
    /// reported. Unreachable owners remain fenced.
    /// # Errors
    /// Reports durable-state errors. Individual unavailable runtimes are retained for a later pass.
    pub async fn reconcile_all(self: &Arc<Self>) -> Result<()> {
        self.resolve_recovery().await?;
        let state = self.state()?;
        let mut tasks = JoinSet::new();
        let permits = Arc::new(Semaphore::new(8));
        for claim in state.claims.values().filter(|claim| claim.phase != Phase::Released) {
            let request = chunk_proto::v1::ClaimRequest::decode(claim.request.as_slice())?;
            let expired = !claim.activated && crate::now_ms().saturating_sub(claim.created_at_ms) > 60_000;
            let cancel = expired
                || state.moves.get(&request.operation_id).is_some_and(|intent| intent.failure.is_some())
                || (claim.phase == Phase::Reserved && state.sessions[&claim.session].retired)
                || claim.phase == Phase::Withdrawing
                || (claim.assignment.is_none() && state.released(&state.sessions[&claim.session].host));
            if !cancel {
                continue;
            }
            let control = self.clone();
            let permits = permits.clone();
            tasks.spawn(async move {
                let Ok(_permit) = permits.acquire_owned().await else {
                    return;
                };
                if let Err(error) = control.cancel(request).await {
                    tracing::debug!(%error, "control reconciliation retains unresolved claim");
                }
            });
        }
        self.join_progressing_drains(tasks).await?;
        self.reconcile_sessions().await?;
        self.retire_idle_hosts()?;
        self.progress_drains()?;
        Ok(())
    }
}
