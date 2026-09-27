//! The operator's methods, `chunk:move_player` and `chunk:drain`, which only the CLI's credential may call. Each is
//! transactional under a client-chosen operation ID, and runs as an accepted control operation, so shutdown refuses
//! new ones and awaits running ones.

use super::{
    super::{
        SyncService, app,
        auth::{Class, Principal},
        errors, position,
    },
    decode,
};
use chunk_proto::{
    sync::v1::{CallRequest, DrainArguments, Error, MovePlayerArguments, MovePlayerResult, Position},
    v1 as control,
};
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
    if request.operation_id.is_empty() || request.operation_id.len() > OPERATION_BYTES {
        return Err(errors::invalid("an operator method requires an operation ID of at most 128 bytes"));
    }
    app::reject_prepared(&request.operation_id)?;
    let control = service.control.clone();
    let operation = request.operation_id.clone();
    let result = match method {
        Method::MovePlayer => {
            let MovePlayerArguments { player, destination } = decode(&request.arguments)?;
            let demand = destination.map(|demand| control::SessionDemand {
                key: demand.key,
                session_type: demand.session_type,
                machine_profile: demand.machine_profile,
            });
            let request =
                control::MovePlayerRequest { operation_id: operation, player_id: player, demand, ..Default::default() };
            let moved = service.operations.admit(async move { control.move_player(request) }).await;
            moved.map(|_| MovePlayerResult {}.encode_to_vec())
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
