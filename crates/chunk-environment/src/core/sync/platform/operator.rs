//! The operator's methods, `chunk:move_player` and `chunk:drain`, which only the CLI's credential may call. Each is
//! transactional under a client-chosen `operator:` operation ID, which control binds to the method and arguments, and
//! runs as an accepted control operation, so shutdown refuses new ones and awaits running ones.

use super::{
    super::{
        SyncService, app,
        auth::{Class, Principal},
        errors, position,
    },
    decode,
};
use chunk_proto::sync::v1::{CallRequest, DrainArguments, Error, MovePlayerArguments, MovePlayerResult, Position};
use chunk_store::Revision;
use prost::Message;

/// Operation IDs name control's moves and drains, whose IDs are at most 128 bytes.
const OPERATION_BYTES: usize = 128;

#[derive(Clone, Copy)]
pub(super) enum Method {
    MovePlayer,
    Drain,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "move_player" => Self::MovePlayer,
            "drain" => Self::Drain,
            _ => return None,
        })
    }
}

/// Runs `method` for the operator, returning its encoded result and control's position after it.
pub(super) async fn call(
    service: &SyncService,
    principal: &Principal,
    method: Method,
    request: &CallRequest,
) -> Result<(Option<Position>, Vec<u8>), Error> {
    if principal.class != Class::Cli {
        return Err(errors::denied("only the operator moves players and drains hosts"));
    }
    if !request.deployment.is_empty() || request.caller.is_some() || !request.stream.is_empty() {
        return Err(errors::invalid("an operator method takes no deployment, caller or stream"));
    }
    if !request.operation_id.starts_with(app::OPERATOR) || request.operation_id.len() > OPERATION_BYTES {
        return Err(errors::invalid(
            "an operator method requires an operation ID of at most 128 bytes beginning with operator:",
        ));
    }
    let control = service.control.clone();
    let operation = request.operation_id.clone();
    let result = match method {
        Method::MovePlayer => {
            let arguments: MovePlayerArguments = decode(&request.arguments)?;
            let moved = service.operations.admit(async move { control.move_operator(&operation, &arguments) }).await;
            moved.map(|()| MovePlayerResult {}.encode_to_vec())
        }
        Method::Drain => {
            let arguments: DrainArguments = decode(&request.arguments)?;
            let drained = service.operations.admit(async move { control.drain_operator(&operation, &arguments) }).await;
            drained.map(|drained| drained.encode_to_vec())
        }
    };
    let result = result.map_err(|failure| errors::operation(&failure))?;
    let generation = *service.control.subscribe().borrow();
    Ok((position(generation.epoch, Revision(generation.revision)), result))
}
