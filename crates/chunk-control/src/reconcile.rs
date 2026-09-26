use std::sync::Arc;

use prost::Message;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{Control, Result, state::Phase};

impl Control {
    /// Refreshes surviving bindings and expires unactivated reservations. Unreachable owners remain fenced.
    /// # Errors
    /// Reports durable-state errors. Individual unavailable runtimes are retained for a later pass.
    pub async fn reconcile_all(self: &Arc<Self>) -> Result<()> {
        self.resolve_recovery().await?;
        let state = self.state()?;
        let stopped: Vec<_> = state.hosts.keys().filter(|id| self.host.stopped(id)).cloned().collect();
        for id in &stopped {
            self.update(|state| {
                state.retire_stopped_host(id);
                Ok(())
            })?;
        }
        let mut tasks = JoinSet::new();
        let permits = Arc::new(Semaphore::new(8));
        for claim in state.claims.values().filter(|claim| claim.phase != Phase::Released) {
            let request = chunk_proto::v1::ClaimRequest::decode(claim.request.as_slice())?;
            let expired = !claim.activated && crate::now_ms().saturating_sub(claim.created_at_ms) > 60_000;
            let cancel = expired
                || state.moves.get(&request.operation_id).is_some_and(|intent| intent.failure.is_some())
                || (claim.phase == Phase::Reserved && state.sessions[&claim.session].retired)
                || claim.phase == Phase::Withdrawing
                || (claim.assignment.is_none() && stopped.contains(&state.sessions[&claim.session].host));
            let control = self.clone();
            let permits = permits.clone();
            tasks.spawn(async move {
                let Ok(_permit) = permits.acquire_owned().await else {
                    return;
                };
                let result = if cancel {
                    control.cancel(request).await.map(|_| ())
                } else {
                    control.inspect(request).await.map(|_| ())
                };
                if let Err(error) = result {
                    tracing::debug!(%error, "control reconciliation retains unresolved claim");
                }
            });
        }
        self.join_progressing_drains(tasks).await?;
        self.reconcile_sessions().await?;
        self.retire_idle_hosts()?;
        self.progress_drains().await?;
        Ok(())
    }

    /// Refreshes activated claims from their runtimes, so watchers see arrivals without waiting for a full pass.
    /// # Errors
    /// Reports unreadable control state. Unavailable runtimes are retried on the next call.
    pub async fn reconcile_arrivals(self: &Arc<Self>) -> Result<()> {
        let state = self.state()?;
        let mut tasks = JoinSet::new();
        let permits = Arc::new(Semaphore::new(8));
        for claim in state.claims.values().filter(|claim| matches!(claim.phase, Phase::Activating | Phase::Attached)) {
            let request = chunk_proto::v1::ClaimRequest::decode(claim.request.as_slice())?;
            let (control, permits) = (self.clone(), permits.clone());
            tasks.spawn(async move {
                let Ok(_permit) = permits.acquire_owned().await else {
                    return;
                };
                if let Err(error) = control.inspect(request).await {
                    tracing::debug!(%error, "claim arrival unresolved");
                }
            });
        }
        tasks.join_all().await;
        Ok(())
    }
}
