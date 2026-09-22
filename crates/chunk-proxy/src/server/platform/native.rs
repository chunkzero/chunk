use std::{io, sync::Arc};

use chunk_contract::{DomainManifest, HookEvent};
use chunk_proto::v1::{ClaimRequest, InvokeHook, SessionDemand, backend_hooks_client::BackendHooksClient};
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tonic::transport::Channel;

use super::{Admission, Platform, RPC_TIMEOUT, Route, Status, channel, invalid_data};

mod lifecycle;
pub(in crate::server) use lifecycle::Lifecycle;

#[derive(Clone)]
pub(super) struct Native {
    client: BackendHooksClient<Channel>,
    manifest: Arc<OnceCell<Option<Arc<DomainManifest>>>>,
}

impl Native {
    pub(super) fn new(endpoint: &str) -> io::Result<Self> {
        Ok(Self { client: BackendHooksClient::new(channel(endpoint)?), manifest: Arc::default() })
    }
}

impl Platform {
    pub(in crate::server) async fn manifest(&self) -> io::Result<Option<Arc<DomainManifest>>> {
        self.native
            .manifest
            .get_or_try_init(|| async {
                let request = self.backend_request((), &self.target.backend.token)?;
                let response = match self.native.client.clone().manifest(request).await {
                    Ok(response) => response.into_inner(),
                    Err(error)
                        if error.code() == tonic::Code::Unimplemented
                            && self.target.backend.platform_token.is_none() =>
                    {
                        return Ok(None);
                    }
                    Err(error) => return Err(io::Error::other(error)),
                };
                if response.deployment != self.target.backend.deployment {
                    return Err(invalid_data("hook manifest deployment mismatch"));
                }
                if response.manifest_json.is_empty() {
                    return Ok(None);
                }
                if self.target.backend.platform_token.is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "native hooks require platform authority",
                    ));
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
            let caller = caller(claim, None);
            self.run_hooks(&manifest, HookEvent::PlayerLogin, &[String::new()], &payload, &caller).await?;
            let routed = self
                .run_hooks(&manifest, HookEvent::PlayerRoute, &[String::new()], &payload, &caller)
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
            self.run_hooks(&manifest, HookEvent::PlayerLogin, &scopes[1..], &payload, &caller).await?;
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
            let caller = caller(destination, destination.source.as_ref());
            // Entry authorization is fresh for every move, including common ancestors.
            self.run_hooks(&manifest, HookEvent::PlayerLogin, &scopes, &payload, &caller).await?;
            self.run_hooks(&manifest, HookEvent::PlayerBeforeMove, &scopes, &payload, &caller).await?;
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
                &json!({"kind":"proxy","proxyId":self.proxy_id}),
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
        caller: &Value,
    ) -> io::Result<Option<Value>> {
        let mut result = None;
        for scope in scopes {
            let mut hooks: Vec<_> =
                manifest.hooks.iter().filter(|(_, hook)| hook.event == event && hook.domain == *scope).collect();
            hooks.sort_by_key(|(id, hook)| (hook.order.unwrap_or_default(), *id));
            for (id, _) in hooks {
                let mut payload = payload.clone();
                payload["domain"] = scope.clone().into();
                let value = self.invoke_hook(id, event, payload, caller.clone()).await?;
                if event.admission() {
                    let admission: Admission = serde_json::from_value(value.clone()).map_err(invalid_data)?;
                    allow(admission)?;
                }
                result = Some(value);
            }
        }
        Ok(result)
    }

    async fn invoke_hook(&self, id: &str, event: HookEvent, payload: Value, caller: Value) -> io::Result<Value> {
        let capacity = if event == HookEvent::ServerPing { &self.status_hooks } else { &self.hooks };
        let _permit = capacity.try_acquire().map_err(|_| io::Error::other("native hook capacity exhausted"))?;
        let token = self
            .target
            .backend
            .platform_token
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::PermissionDenied, "missing platform hook authority"))?;
        let mut caller = caller;
        caller["environment"] = self.target.backend.environment.clone().into();
        caller["deployment"] = self.target.backend.deployment.clone().into();
        let request = self.backend_request(
            InvokeHook {
                hook: id.into(),
                arguments_json: serde_json::to_vec(&payload).map_err(invalid_data)?,
                caller_json: serde_json::to_vec(&caller).map_err(invalid_data)?,
            },
            token,
        )?;
        let response = self.native.client.clone().invoke(request).await.map_err(io::Error::other)?.into_inner();
        serde_json::from_slice(&response.result_json).map_err(invalid_data)
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

fn caller(claim: &ClaimRequest, identity: Option<&chunk_proto::v1::ClaimIdentity>) -> Value {
    json!({"kind":"proxy","proxyId":claim.proxy_id,"connectionId":claim.connection_id,"operationId":claim.operation_id,
        "membershipGeneration":identity.map(|id|id.membership_generation.to_string()),
        "deliveryGeneration":identity.map(|id|id.delivery_generation.to_string())})
}

fn demand_json(demand: &SessionDemand) -> Value {
    json!({"key":demand.key,"session_type":demand.session_type,"machine_profile":demand.machine_profile})
}

fn allow(admission: Admission) -> io::Result<()> {
    if admission.allow {
        return Ok(());
    }
    let reason = admission.reason.unwrap_or_else(|| "Admission denied.".into());
    Err(io::Error::new(io::ErrorKind::PermissionDenied, reason.chars().take(256).collect::<String>()))
}

#[cfg(test)]
mod tests;
