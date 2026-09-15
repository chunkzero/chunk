use chunk_proto::v1::ClaimIdentity;
use tokio_util::sync::CancellationToken;

use super::{
    Arc, ClaimRequest, DomainManifest, HookEvent, Platform, ancestors, caller, domain, invalid_data, io, payload,
    request,
};

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
        for (event, scope) in events {
            let origin = if event == HookEvent::DomainLeave {
                self.arrived.as_ref().map(|arrival| (arrival.claim.clone(), arrival.identity.clone()))
            } else {
                None
            }
            .unwrap_or_else(|| (claim.clone(), identity.clone()));
            self.platform.notify(manifest, (event, scope), origin, &self.session, &self.connection);
        }
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
                let response = platform
                    .control
                    .clone()
                    .reconcile_departure(request(claim.clone(), &platform.target.control.token)?)
                    .await
                    .map_err(io::Error::other)?
                    .into_inner();
                let same_membership = response.claim.as_ref().is_some_and(|identity| {
                    identity.membership_generation == arrival.identity.membership_generation
                        && identity.operation_id == claim.operation_id
                        && identity.proxy_id == claim.proxy_id
                });
                if response.departed && same_membership {
                    let scopes = ancestors(&arrival.domain);
                    let events = scopes
                        .iter()
                        .rev()
                        .cloned()
                        .map(|scope| (HookEvent::DomainLeave, scope))
                        .chain(scopes.iter().cloned().rev().map(|scope| (HookEvent::PlayerDisconnect, scope)))
                        .collect::<Vec<_>>();
                    for event in events {
                        platform.notify(
                            &arrival.manifest,
                            event,
                            (arrival.claim.clone(), arrival.identity.clone()),
                            &CancellationToken::new(),
                            &CancellationToken::new(),
                        );
                    }
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

impl Platform {
    fn notify(
        &self,
        manifest: &DomainManifest,
        (event, scope): (HookEvent, String),
        (claim, identity): (ClaimRequest, ClaimIdentity),
        session: &CancellationToken,
        connection: &CancellationToken,
    ) {
        let Ok(mut payload) = payload(&claim) else {
            return;
        };
        payload["domain"] = scope.clone().into();
        if event == HookEvent::PlayerDisconnect {
            payload["reason"] = "connection closed".into();
        }
        let caller = caller(&claim, Some(&identity));
        for (id, hook) in &manifest.hooks {
            if hook.event != event || hook.domain != scope {
                continue;
            }
            let platform = self.clone();
            let id = id.clone();
            let payload = payload.clone();
            let caller = caller.clone();
            let cancellation = if hook.follow_player { connection.clone() } else { session.clone() };
            let connection = connection.clone();
            self.cleanup.spawn(async move {
                tokio::select! {
                    biased;
                    () = connection.cancelled() => {},
                    () = cancellation.cancelled() => {},
                    result = platform.invoke_hook(&id,event,payload,caller) => {
                        if let Err(error) = result { tracing::warn!(%error,hook=id,"lifecycle notification failed"); }
                    }
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notifications_include_only_changed_ancestry_after_initial_connection() {
        use HookEvent::{DomainEnter, DomainLeave, PlayerConnect};
        assert_eq!(
            transition(None, "games/lobby"),
            vec![
                (PlayerConnect, String::new()),
                (PlayerConnect, "games".into()),
                (PlayerConnect, "games/lobby".into()),
                (DomainEnter, String::new()),
                (DomainEnter, "games".into()),
                (DomainEnter, "games/lobby".into())
            ]
        );
        assert_eq!(
            transition(Some("games/lobby/deep"), "games/match"),
            vec![
                (DomainLeave, "games/lobby/deep".into()),
                (DomainLeave, "games/lobby".into()),
                (DomainEnter, "games/match".into())
            ]
        );
        assert!(transition(Some("games/lobby"), "games/lobby").is_empty());
    }
}
