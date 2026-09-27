//! Platform methods, named `chunk:<name>`, whose arguments and results are protobuf messages.

mod claims;
mod commands;
mod jvm;

use super::{
    SyncService, app,
    auth::{Class, Principal},
    errors, position,
};
use chunk_js::DeploymentId;
use chunk_proto::sync::v1::{CallRequest, Error, ManifestResult, Position, PrepareResult, error::Code};
use chunk_store::Revision;
use prost::Message;

/// Runs platform method `method` for `principal`, returning its encoded result and, for claim methods, control's
/// position after it.
pub(super) async fn call(
    service: &SyncService,
    principal: &Principal,
    method: &str,
    request: &CallRequest,
) -> Result<(Option<Position>, Vec<u8>), Error> {
    match method {
        "prepare" => return Ok((None, prepare(service, request).await?.encode_to_vec())),
        "manifest" => return Ok((None, manifest(service, principal, request).await?.encode_to_vec())),
        _ => {}
    }
    if let Some(method) = jvm::Method::parse(method) {
        return jvm::call(service, principal, method, request).await;
    }
    if let Some(method) = commands::Method::parse(method) {
        let Class::Gateway { id } = &principal.class else {
            return Err(errors::denied("only a gateway runs commands"));
        };
        if !request.deployment.is_empty() {
            return Err(errors::invalid("a command method takes no deployment"));
        }
        service.fences.check(&request.stream, &principal.credential)?;
        return Ok((None, commands::call(service, id, &principal.credential, method, request).await?));
    }
    let method = claims::Method::parse(method).ok_or_else(|| errors::invalid("unknown method"))?;
    let Class::Gateway { id } = &principal.class else {
        return Err(errors::denied("only a gateway manages claims"));
    };
    if !request.deployment.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("a claim method takes no deployment or caller"));
    }
    if request.operation_id.is_empty() {
        return Err(errors::invalid("a claim method requires an operation ID"));
    }
    app::reject_prepared(&request.operation_id)?;
    service.fences.check(&request.stream, &principal.credential)?;
    let result = claims::call(service, id, method, request).await?;
    let generation = *service.control.subscribe().borrow();
    Ok((position(generation.epoch, Revision(generation.revision)), result))
}

/// Issues the operation ID of one effectful call.
async fn prepare(service: &SyncService, request: &CallRequest) -> Result<PrepareResult, Error> {
    if !request.operation_id.is_empty()
        || !request.arguments.is_empty()
        || !request.deployment.is_empty()
        || request.caller.is_some()
    {
        return Err(errors::invalid("chunk:prepare takes no operation ID, arguments, deployment or caller"));
    }
    let id = service.app.backend().allocate_action_id().await.map_err(|failure| errors::backend(&failure))?;
    Ok(PrepareResult { operation_id: format!("{}{id}", app::PREPARED) })
}

/// The domain manifest of the deployment `request` names, or of the current release's.
async fn manifest(
    service: &SyncService,
    principal: &Principal,
    request: &CallRequest,
) -> Result<ManifestResult, Error> {
    if !matches!(principal.class, Class::Gateway { .. }) {
        return Err(errors::denied("only a gateway reads the domain manifest"));
    }
    if !request.arguments.is_empty() || request.caller.is_some() {
        return Err(errors::invalid("chunk:manifest takes no arguments or caller"));
    }
    let deployment = if request.deployment.is_empty() {
        let current = service.control.current_release().map_err(|failure| errors::control(&failure))?;
        current.ok_or_else(|| errors::error(Code::Contract, "no release is current"))?
    } else {
        request.deployment.clone()
    };
    let id = DeploymentId::new(&deployment).map_err(|_| errors::invalid("invalid deployment"))?;
    let manifest = service.app.backend().domain_manifest(id).await.map_err(|failure| errors::backend(&failure))?;
    let manifest_json = manifest.map(|manifest| serde_json::to_vec(&manifest)).transpose();
    let manifest_json = manifest_json.map_err(|_| errors::error(Code::Contract, "invalid domain manifest"))?;
    Ok(ManifestResult { deployment, manifest_json: manifest_json.unwrap_or_default() })
}

fn decode<T: Message + Default>(arguments: &[u8]) -> Result<T, Error> {
    T::decode(arguments).map_err(|_| errors::invalid("arguments are not the method's message"))
}
