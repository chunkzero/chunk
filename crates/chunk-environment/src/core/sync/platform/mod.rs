//! Platform methods, named `chunk:<name>`, whose arguments and results are protobuf messages.

mod claims;

use super::{
    SyncService,
    auth::{Class, Principal},
    errors, position,
};
use chunk_proto::sync::v1::{CallRequest, Error, Position};
use chunk_store::Revision;

/// Runs platform method `method` for `principal`, returning its encoded result and control's position after it.
pub(super) async fn call(
    service: &SyncService,
    principal: &Principal,
    method: &str,
    request: &CallRequest,
) -> Result<(Option<Position>, Vec<u8>), Error> {
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
    service.fences.check(&request.stream, &principal.credential)?;
    let result = claims::call(service, id, method, request).await?;
    let generation = *service.control.subscribe().borrow();
    Ok((position(generation.epoch, Revision(generation.revision)), result))
}

/// Runs `operation` on control's tracker, so a dropped call doesn't abandon it midway and control awaits it before
/// stopping.
async fn run<T: Send + 'static>(
    service: &SyncService,
    operation: impl Future<Output = chunk_control::Result<T>> + Send + 'static,
) -> chunk_control::Result<T> {
    let outcome = service.operations.spawn(operation).await;
    outcome.unwrap_or(Err(chunk_control::Error::Unresolved("the control operation's task failed")))
}
