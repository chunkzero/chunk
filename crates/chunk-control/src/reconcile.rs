use std::{sync::Arc, time::Duration};

use prost::Message;
use tokio::{sync::Semaphore, task::JoinSet};

use crate::{Control, Result, state::Phase};

impl Control {
    /// Refreshes surviving bindings and expires unactivated reservations. Unreachable owners remain fenced.
    /// # Errors
    /// Reports durable-state errors. Individual unavailable runtimes are retained for a later pass.
    pub async fn reconcile_all(self: &Arc<Self>) -> Result<()> {
        let state = self.state()?;
        let stopped: Vec<_> = state.hosts.keys().filter(|id| self.host.stopped(id)).cloned().collect();
        if !stopped.is_empty() {
            self.update(|state| {
                for id in &stopped {
                    if let Some(host) = state.hosts.get_mut(id) {
                        host.retired = true;
                    }
                    for session in state.sessions.values_mut().filter(|session| &session.host == id) {
                        session.retired = true;
                    }
                }
                Ok(())
            })?;
        }
        let mut tasks = JoinSet::new();
        let permits = Arc::new(Semaphore::new(8));
        for claim in state.claims.values().filter(|claim| claim.phase != Phase::Released) {
            let request = chunk_proto::v1::ClaimRequest::decode(claim.request.as_slice())?;
            let expired = !claim.activated && crate::now_ms().saturating_sub(claim.created_at_ms) > 60_000;
            let cancel = expired
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
        let mut drains = tokio::time::interval(Duration::from_secs(1));
        while !tasks.is_empty() {
            tokio::select! {
                _ = tasks.join_next() => {}
                _ = drains.tick() => self.progress_drains().await?,
            }
        }
        self.progress_drains().await?;
        Ok(())
    }
}
