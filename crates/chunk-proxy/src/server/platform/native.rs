use std::{io, sync::Arc};

use chunk_contract::{DomainManifest, HookEvent};
use chunk_proto::{
    sync::v1::{CallRequest, ManifestResult},
    v1::{ClaimRequest, SessionDemand},
};
use prost::Message;
use serde_json::{Value, json};

use super::{Admission, Platform, RPC_TIMEOUT, Route, Status, invalid_data};

mod lifecycle;
pub(in crate::server) use lifecycle::Lifecycle;

impl Platform {
    /// The domain manifest of the target's deployment, or `None` when it declares none and the app's legacy
    /// `shared/proxy/*` hooks apply.
    pub(in crate::server) async fn manifest(&self) -> io::Result<Option<Arc<DomainManifest>>> {
        self.manifest
            .get_or_try_init(|| async {
                let deployment = &self.target.backend.deployment;
                let message = CallRequest {
                    method: "chunk:manifest".into(),
                    deployment: deployment.clone(),
                    ..CallRequest::default()
                };
                let response = self.sync.unfenced(message).await?;
                let response = ManifestResult::decode(response.as_slice()).map_err(invalid_data)?;
                if response.deployment != *deployment {
                    return Err(invalid_data("hook manifest deployment mismatch"));
                }
                if response.manifest_json.is_empty() {
                    return Ok(None);
                }
                let manifest: DomainManifest = serde_json::from_slice(&response.manifest_json).map_err(invalid_data)?;
                manifest.validate().map_err(invalid_data)?;
                Ok(Some(Arc::new(manifest)))
            })
            .await
            .cloned()
    }

    pub(in crate::server) async fn route_claim(&self, claim: &ClaimRequest) -> io::Result<SessionDemand> {
        tokio::time::timeout(RPC_TIMEOUT, async {
            let Some(manifest) = self.manifest().await? else {
                let identity = claim.identity.as_ref().ok_or_else(|| invalid_data("missing player identity"))?;
                return self.legacy_route(&identity.uuid, &identity.username).await;
            };
            let mut payload = payload(claim)?;
            payload["destination"] = Value::Null;
            self.run_hooks(&manifest, HookEvent::PlayerLogin, &[String::new()], &payload, None).await?;
            let routed = self
                .run_hooks(&manifest, HookEvent::PlayerRoute, &[String::new()], &payload, None)
                .await?
                .ok_or_else(|| invalid_data("native domains require a root routing responder"))?;
            let route: Route = serde_json::from_value(routed.clone()).map_err(invalid_data)?;
            let demand = SessionDemand {
                key: route.key,
                session_type: route.session_type,
                machine_profile: route.machine_profile,
            };
            let domain = domain(&manifest, &demand)?;
            payload["destination"] = routed;
            let scopes = ancestors(domain);
            self.run_hooks(&manifest, HookEvent::PlayerLogin, &scopes[1..], &payload, None).await?;
            Ok(demand)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(in crate::server) async fn approve_move(
        &self,
        source: &ClaimRequest,
        destination: &ClaimRequest,
    ) -> io::Result<()> {
        tokio::time::timeout(RPC_TIMEOUT, async {
            let Some(manifest) = self.manifest().await? else {
                return self.legacy_approve_move(destination).await;
            };
            let demand = destination.demand.as_ref().ok_or_else(|| invalid_data("missing move destination"))?;
            let source_demand = source.demand.as_ref().ok_or_else(|| invalid_data("missing source destination"))?;
            let mut payload = payload(destination)?;
            payload["destination"] = demand_json(demand);
            payload["sourceDomain"] = domain(&manifest, source_demand)?.into();
            let scopes = ancestors(domain(&manifest, demand)?);
            // The player holds their source claim throughout.
            let player = destination.identity.as_ref().map(|identity| identity.uuid.as_str());
            // Entry authorization is fresh for every move, including common ancestors.
            self.run_hooks(&manifest, HookEvent::PlayerLogin, &scopes, &payload, player).await?;
            self.run_hooks(&manifest, HookEvent::PlayerBeforeMove, &scopes, &payload, player).await?;
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(super) async fn native_status(&self, manifest: &DomainManifest, host: &str) -> io::Result<Status> {
        let result = self
            .run_hooks(
                manifest,
                HookEvent::ServerPing,
                &[String::new()],
                &json!({"eventId":uuid::Uuid::new_v4().to_string(),"host":host}),
                None,
            )
            .await?
            .ok_or_else(|| invalid_data("native domains require a ping responder"))?;
        serde_json::from_value(result).map_err(invalid_data)
    }

    async fn run_hooks(
        &self,
        manifest: &DomainManifest,
        event: HookEvent,
        scopes: &[String],
        payload: &Value,
        player: Option<&str>,
    ) -> io::Result<Option<Value>> {
        let mut result = None;
        for scope in scopes {
            let mut hooks: Vec<_> =
                manifest.hooks.iter().filter(|(_, hook)| hook.event == event && hook.domain == *scope).collect();
            hooks.sort_by_key(|(id, hook)| (hook.order.unwrap_or_default(), *id));
            for (id, _) in hooks {
                let mut payload = payload.clone();
                payload["domain"] = scope.clone().into();
                let value = self.invoke_hook(id, event, &payload, player).await?;
                if event.admission() {
                    let admission: Admission = serde_json::from_value(value.clone()).map_err(invalid_data)?;
                    admission.check()?;
                }
                result = Some(value);
            }
        }
        Ok(result)
    }

    /// Runs hook `id` as an effectful call, naming `player` as the caller while this gateway holds their claim.
    async fn invoke_hook(
        &self,
        id: &str,
        event: HookEvent,
        payload: &Value,
        player: Option<&str>,
    ) -> io::Result<Value> {
        let capacity = if event == HookEvent::ServerPing { &self.status_hooks } else { &self.hooks };
        let _permit = capacity.try_acquire().map_err(|_| io::Error::other("native hook capacity exhausted"))?;
        let operation = self.prepare().await?;
        self.call_app(operation, id, payload, player).await
    }
}

fn domain<'a>(manifest: &'a DomainManifest, demand: &SessionDemand) -> io::Result<&'a str> {
    let (app, _) =
        demand.session_type.split_once('/').ok_or_else(|| invalid_data("destination requires app/session_type"))?;
    manifest
        .apps
        .get(app)
        .map(String::as_str)
        .ok_or_else(|| invalid_data("destination app absent from domain manifest"))
}

fn ancestors(domain: &str) -> Vec<String> {
    let mut result = vec![String::new()];
    for (end, _) in domain.match_indices('/') {
        result.push(domain[..end].into());
    }
    if !domain.is_empty() {
        result.push(domain.into());
    }
    result
}

fn payload(claim: &ClaimRequest) -> io::Result<Value> {
    let player = claim.identity.as_ref().ok_or_else(|| invalid_data("missing authenticated identity"))?;
    Ok(json!({"eventId":claim.operation_id,"player":{"uuid":player.uuid,"username":player.username}}))
}

fn demand_json(demand: &SessionDemand) -> Value {
    json!({"key":demand.key,"session_type":demand.session_type,"machine_profile":demand.machine_profile})
}

#[cfg(test)]
mod tests;
