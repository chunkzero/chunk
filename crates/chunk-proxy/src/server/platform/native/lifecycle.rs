use std::{io, sync::Arc};

use chunk_contract::{DomainManifest, HookEvent};
use chunk_proto::{
    sync::v1::DepartResult,
    v1::{ClaimIdentity, ClaimRequest},
};
use tokio_util::sync::CancellationToken;

use super::{Platform, RPC_TIMEOUT, ancestors, caller, domain, invalid_data, payload};

/// One socket's captured domain and membership; each invocation receives fresh capabilities.
pub(in crate::server) struct Lifecycle {
    platform: Platform,
    arrived: Option<Arrival>,
    cleanup_claim: Option<ClaimRequest>,
    connection: CancellationToken,
    session: CancellationToken,
}

struct Arrival {
    manifest: Arc<DomainManifest>,
    domain: String,
    claim: ClaimRequest,
    identity: ClaimIdentity,
}

impl Lifecycle {
    pub(in crate::server) fn new(platform: Platform) -> Self {
        Self {
            platform,
            arrived: None,
            cleanup_claim: None,
            connection: CancellationToken::new(),
            session: CancellationToken::new(),
        }
    }

    pub(in crate::server) fn cutover(&mut self, claim: &ClaimRequest) {
        self.session.cancel();
        self.session = CancellationToken::new();
        self.cleanup_claim = Some(claim.clone());
    }

    pub(in crate::server) fn arrived(&mut self, claim: &ClaimRequest, identity: &ClaimIdentity) -> io::Result<()> {
        let Some(Some(manifest)) = self.platform.native.manifest.get() else {
            return Ok(());
        };
        let demand = claim.demand.as_ref().ok_or_else(|| invalid_data("missing arrived destination"))?;
        let domain = domain(manifest, demand)?.to_owned();
        let events = transition(self.arrived.as_ref().map(|arrival| arrival.domain.as_str()), &domain);
        let mut notifications = Vec::new();
        for (event, scope) in events {
            let origin = if event == HookEvent::DomainLeave {
                self.arrived.as_ref().map(|arrival| (arrival.claim.clone(), arrival.identity.clone()))
            } else {
                None
            }
            .unwrap_or_else(|| (claim.clone(), identity.clone()));
            notifications.push(((event, scope), origin));
        }
        self.platform.notify(manifest, notifications, &self.session, &self.connection);
        self.arrived =
            Some(Arrival { manifest: manifest.clone(), domain, claim: claim.clone(), identity: identity.clone() });
        self.cleanup_claim = Some(claim.clone());
        Ok(())
    }
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        self.connection.cancel();
        self.session.cancel();
        let Some(arrival) = self.arrived.take() else {
            return;
        };
        let Some(claim) = self.cleanup_claim.take() else {
            return;
        };
        let platform = self.platform.clone();
        self.platform.cleanup.spawn(async move {
            let result = async {
                let operation = &claim.operation_id;
                let (result, _): (DepartResult, _) = platform.call("depart", operation, &(), RPC_TIMEOUT).await?;
                if result.departed {
                    let scopes = ancestors(&arrival.domain);
                    let events = scopes
                        .iter()
                        .rev()
                        .cloned()
                        .map(|scope| (HookEvent::DomainLeave, scope))
                        .chain(scopes.iter().cloned().rev().map(|scope| (HookEvent::PlayerDisconnect, scope)))
                        .collect::<Vec<_>>();
                    platform.notify(
                        &arrival.manifest,
                        events
                            .into_iter()
                            .map(|event| (event, (arrival.claim.clone(), arrival.identity.clone())))
                            .collect(),
                        &CancellationToken::new(),
                        &CancellationToken::new(),
                    );
                }
                Ok::<_, io::Error>(())
            }
            .await;
            if let Err(error) = result {
                tracing::warn!(%error,"logical departure remains unresolved");
            }
        });
    }
}

fn transition(previous: Option<&str>, next: &str) -> Vec<(HookEvent, String)> {
    let scopes = ancestors(next);
    let Some(previous) = previous else {
        return scopes
            .iter()
            .cloned()
            .map(|scope| (HookEvent::PlayerConnect, scope))
            .chain(scopes.iter().cloned().map(|scope| (HookEvent::DomainEnter, scope)))
            .collect();
    };
    let old = ancestors(previous);
    let common = old.iter().zip(&scopes).take_while(|(left, right)| left == right).count();
    old[common..]
        .iter()
        .rev()
        .cloned()
        .map(|scope| (HookEvent::DomainLeave, scope))
        .chain(scopes[common..].iter().cloned().map(|scope| (HookEvent::DomainEnter, scope)))
        .collect()
}

type Notification = ((HookEvent, String), (ClaimRequest, ClaimIdentity));

impl Platform {
    fn notify(
        &self,
        manifest: &DomainManifest,
        notifications: Vec<Notification>,
        session: &CancellationToken,
        connection: &CancellationToken,
    ) {
        let mut calls = Vec::new();
        for ((event, scope), (claim, identity)) in notifications {
            let Ok(mut payload) = payload(&claim) else {
                continue;
            };
            payload["domain"] = scope.clone().into();
            if event == HookEvent::PlayerDisconnect {
                payload["reason"] = "connection closed".into();
            }
            let caller = caller(&claim, Some(&identity));
            for (id, hook) in &manifest.hooks {
                if hook.event == event && hook.domain == scope {
                    calls.push((id.clone(), event, payload.clone(), caller.clone(), hook.follow_player));
                }
            }
        }
        if calls.is_empty() {
            return;
        }
        let platform = self.clone();
        let session = session.clone();
        let connection = connection.clone();
        self.cleanup.spawn(async move {
            // One bounded batch preserves ancestry order without holding a transaction.
            let batch = async {
                for (id, event, payload, caller, follow_player) in calls {
                    let cancellation = if follow_player { &connection } else { &session };
                    tokio::select! {
                        biased;
                        () = connection.cancelled() => return,
                        () = cancellation.cancelled() => {},
                        result = platform.invoke_hook(&id, event, payload, caller) => {
                            if let Err(error) = result { tracing::warn!(%error,hook=id,"lifecycle notification failed"); }
                        }
                    }
                }
            };
            if tokio::time::timeout(RPC_TIMEOUT, batch).await.is_err() {
                tracing::warn!("lifecycle notification batch deadline exceeded");
            }
        });
    }
}

#[cfg(test)]
mod tests;
