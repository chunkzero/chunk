use std::time::Duration;

use chunk_contract::{Deployment, Field, Function, FunctionKind, HookEvent, Schema, Visibility};
use chunk_proto::v1::{HookManifest, HookResult, InvokeHook, backend_hooks_server};
use serde_json::Value;
use tonic::{Request, Response, Status};

use crate::{Backend, Call, Error, Result, Service, transport::status};

pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

/// Metadata is readable with the application credential; lifecycle invocation
/// requires a separate credential held only by trusted platform processes.
#[derive(Clone)]
pub struct HookService {
    backend: Backend,
    application: Service,
    platform: Service,
}

impl HookService {
    /// # Errors
    /// Requires valid, distinct application and platform credentials.
    pub fn new(backend: Backend, application: &str, platform: &str) -> Result<Self> {
        if application == platform {
            return Err(Error::Invalid("hook authority must differ from application authority"));
        }
        Ok(Self {
            application: Service::new(backend.clone(), application)?,
            platform: Service::new(backend.clone(), platform)?,
            backend,
        })
    }

    #[must_use]
    pub fn into_server(self) -> backend_hooks_server::BackendHooksServer<Self> {
        backend_hooks_server::BackendHooksServer::new(self)
            .max_decoding_message_size(2 * 1024 * 1024)
            .max_encoding_message_size(2 * 1024 * 1024)
    }
}

#[tonic::async_trait]
impl backend_hooks_server::BackendHooks for HookService {
    async fn manifest(&self, request: Request<()>) -> std::result::Result<Response<HookManifest>, Status> {
        let deployment = self.application.binding(&request)?;
        let manifest = self.backend.domain_manifest(deployment.clone()).await.map_err(|error| status(&error))?;
        let manifest_json = manifest
            .map(|manifest| serde_json::to_vec(&manifest))
            .transpose()
            .map_err(|_| Status::internal("invalid domain manifest"))?
            .unwrap_or_default();
        Ok(Response::new(HookManifest { deployment: deployment.as_str().into(), manifest_json }))
    }

    async fn invoke(&self, request: Request<InvokeHook>) -> std::result::Result<Response<HookResult>, Status> {
        let deployment = self.platform.binding(&request)?;
        let message = request.into_inner();
        // The platform credential is a gateway's authority, and it names no player.
        let call = Service::decode(deployment, message.hook, &message.arguments_json, br#"{"kind":"gateway"}"#)?;
        let result = tokio::time::timeout(HOOK_TIMEOUT, self.backend.invoke_hook(call))
            .await
            .map_err(|_| Status::deadline_exceeded("hook deadline"))?
            .map_err(|error| status(&error))?;
        Ok(Response::new(HookResult { result_json: result.as_bytes().to_vec() }))
    }
}

pub(crate) fn resolve(deployment: &Deployment, call: &Call) -> Result<(Function, bool)> {
    let manifest = deployment.contracts.domains.as_ref().ok_or(Error::Unknown)?;
    let hook = manifest.hooks.get(&call.function).ok_or(Error::Unknown)?;
    let arguments: Value = serde_json::from_str(call.arguments.as_str())?;
    let caller: Value = serde_json::from_str(call.caller.as_str())?;
    if arguments["domain"].as_str() != Some(&hook.domain) || !identifier(&arguments["eventId"]) || !gateway(&caller) {
        return Err(Error::Invalid("invalid trusted hook context"));
    }
    if caller.get("player").is_some() && caller["player"] != arguments["player"]["uuid"] {
        return Err(Error::Invalid("the hook's caller names another player"));
    }
    if hook.event == HookEvent::ServerPing {
        if arguments.get("player").is_some() || !identifier(&arguments["host"]) {
            return Err(Error::Invalid("invalid ping context"));
        }
    } else if !identifier(&arguments["player"]["uuid"]) || !identifier(&arguments["player"]["username"]) {
        return Err(Error::Invalid("invalid hook player identity"));
    }
    match hook.event {
        HookEvent::PlayerLogin => {
            if arguments.get("destination").is_none()
                || (!arguments["destination"].is_null() && !destination().accepts(&arguments["destination"]))
            {
                return Err(Error::Invalid("invalid admission destination"));
            }
        }
        HookEvent::PlayerBeforeMove => {
            if !arguments["sourceDomain"].as_str().is_some_and(|domain| manifest.scopes.contains_key(domain))
                || !destination().accepts(&arguments["destination"])
            {
                return Err(Error::Invalid("invalid move context"));
            }
        }
        HookEvent::PlayerDisconnect if arguments["reason"].as_str().is_none() => {
            return Err(Error::Invalid("missing disconnect reason"));
        }
        _ => {}
    }
    let result = match hook.event {
        HookEvent::ServerPing => {
            object([("motd", Schema::String), ("online", Schema::Integer), ("max", Schema::Integer)])
        }
        HookEvent::PlayerRoute => destination(),
        HookEvent::PlayerLogin | HookEvent::PlayerBeforeMove => Schema::Object {
            fields: [
                ("allow".into(), Field { schema: Schema::Boolean, optional: false }),
                ("reason".into(), Field { schema: Schema::String, optional: true }),
            ]
            .into(),
        },
        _ => Schema::Null,
    };
    Ok((
        Function {
            kind: FunctionKind::Action,
            visibility: Visibility::Internal,
            export: hook.export.clone(),
            arguments: Schema::Null,
            result,
        },
        hook.event != HookEvent::ServerPing,
    ))
}

/// A gateway's caller: `{"kind":"gateway"}`, with the `player` it holds a claim for.
fn gateway(caller: &Value) -> bool {
    caller.as_object().is_some_and(|fields| {
        caller["kind"] == "gateway"
            && fields.keys().all(|key| key == "kind" || key == "player")
            && (!fields.contains_key("player") || identifier(&caller["player"]))
    })
}

fn identifier(value: &Value) -> bool {
    value.as_str().is_some_and(|value| !value.is_empty() && value.len() <= 256 && !value.contains('\0'))
}

fn object<const N: usize>(fields: [(&str, Schema); N]) -> Schema {
    Schema::Object {
        fields: fields.into_iter().map(|(name, schema)| (name.into(), Field { schema, optional: false })).collect(),
    }
}

fn destination() -> Schema {
    object([("key", Schema::String), ("session_type", Schema::String), ("machine_profile", Schema::String)])
}

#[cfg(test)]
mod tests;
