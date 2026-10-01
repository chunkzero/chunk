use std::{io, sync::Arc};

use chunk_contract::{DomainManifest, HookEvent};
use chunk_proto::sync::v1::DepartResult;
use tokio_util::sync::CancellationToken;

use super::{Claim, Platform, RPC_TIMEOUT, ancestors, domain, payload};

/// One socket's captured domain and claim.
pub(in crate::server) struct Lifecycle {
    platform: Platform,
    arrived: Option<Arrival>,
    cleanup_claim: Option<Claim>,
    connection: CancellationToken,
    session: CancellationToken,
}

struct Arrival {
    platform: Platform,
    manifest: Arc<DomainManifest>,
    domain: String,
    claim: Claim,
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

    /// Continues with `platform`, which a move into another deployment's session bound the connection to.
    pub(in crate::server) fn rebind(&mut self, platform: Platform) {
        self.platform = platform;
    }

    pub(in crate::server) fn cutover(&mut self, claim: &Claim) {
        self.session.cancel();
        self.session = CancellationToken::new();
        self.cleanup_claim = Some(claim.clone());
    }

    pub(in crate::server) fn arrived(&mut self, claim: &Claim) -> io::Result<()> {
        let Some(Some(manifest)) = self.platform.manifest.get() else {
            return Ok(());
        };
        let domain = domain(manifest, &claim.demand)?.to_owned();
        if let Some(old) =
            self.arrived.as_ref().filter(|old| old.platform.target.deployment != self.platform.target.deployment)
        {
            // Another deployment's domains share no scopes with this one's.
            let leaves = ancestors(&old.domain)
                .into_iter()
                .rev()
                .map(|scope| ((HookEvent::DomainLeave, scope), old.claim.clone()));
            old.platform.notify(&old.manifest, leaves.collect(), true, &self.session, &self.connection);
            let enters = ancestors(&domain).into_iter().map(|scope| ((HookEvent::DomainEnter, scope), claim.clone()));
            self.platform.notify(manifest, enters.collect(), true, &self.session, &self.connection);
        } else {
            let events = transition(self.arrived.as_ref().map(|arrival| arrival.domain.as_str()), &domain);
            let mut notifications = Vec::new();
            for (event, scope) in events {
                let origin = if event == HookEvent::DomainLeave {
                    self.arrived.as_ref().map(|arrival| arrival.claim.clone())
                } else {
                    None
                }
                .unwrap_or_else(|| claim.clone());
                notifications.push(((event, scope), origin));
            }
            self.platform.notify(manifest, notifications, true, &self.session, &self.connection);
        }
        self.arrived =
            Some(Arrival { platform: self.platform.clone(), manifest: manifest.clone(), domain, claim: claim.clone() });
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
        let platform = arrival.platform.clone();
        platform.cleanup.clone().spawn(async move {
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
                        events.into_iter().map(|event| (event, arrival.claim.clone())).collect(),
                        false,
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

type Notification = ((HookEvent, String), Claim);

impl Platform {
    /// Runs the hooks of `notifications` in order, naming each one's player as the caller while `held`, when this
    /// gateway holds their claim.
    fn notify(
        &self,
        manifest: &DomainManifest,
        notifications: Vec<Notification>,
        held: bool,
        session: &CancellationToken,
        connection: &CancellationToken,
    ) {
        let mut calls = Vec::new();
        for ((event, scope), claim) in notifications {
            let mut payload = payload(&claim);
            payload["domain"] = scope.clone().into();
            if event == HookEvent::PlayerDisconnect {
                payload["reason"] = "connection closed".into();
            }
            let player = held.then(|| claim.player.uuid.clone());
            for (id, hook) in &manifest.hooks {
                if hook.event == event && hook.domain == scope {
                    calls.push((id.clone(), event, payload.clone(), player.clone(), hook.follow_player));
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
                for (id, event, payload, player, follow_player) in calls {
                    let cancellation = if follow_player { &connection } else { &session };
                    tokio::select! {
                        biased;
                        () = connection.cancelled() => return,
                        () = cancellation.cancelled() => {},
                        result = platform.invoke_hook(&id, event, &payload, player.as_deref()) => {
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
