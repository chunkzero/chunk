//! A gateway's claim lifecycle, run through the same control operations as its local proxy's calls: `chunk:claim`,
//! `chunk:activate`, `chunk:withdraw`, `chunk:abandon_move` and `chunk:depart`. The call's operation ID names the
//! claim, which must be the calling gateway's.

use super::{super::SyncService, decode, errors};
use chunk_control::{ALREADY_OWNED, Error as Failure, Generation, ROSTER_WAITING, ROUTE_AGAIN, StoredClaim};
use chunk_proto::{
    sync::v1::{
        AbandonMoveArguments, ActivateResult, CallRequest, ClaimArguments, ClaimAssignment, ClaimRefusal, ClaimResult,
        DepartResult, Error, GatewayLogin, Position, WithdrawResult, claim_result::Outcome, error::Code,
    },
    v1 as control,
};
use prost::Message;

#[derive(Clone, Copy)]
pub(super) enum Method {
    Claim,
    Activate,
    Withdraw,
    AbandonMove,
    Depart,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "claim" => Self::Claim,
            "activate" => Self::Activate,
            "withdraw" => Self::Withdraw,
            "abandon_move" => Self::AbandonMove,
            "depart" => Self::Depart,
            _ => return None,
        })
    }
}

/// Runs `method` for `gateway` on the claim `request`'s operation ID names, returning its encoded result.
pub(super) async fn call(
    service: &SyncService,
    gateway: &str,
    method: Method,
    request: &CallRequest,
) -> Result<Vec<u8>, Error> {
    let operation = request.operation_id.as_str();
    let arguments = request.arguments.as_slice();
    if matches!(method, Method::Activate | Method::Withdraw | Method::Depart) && !arguments.is_empty() {
        return Err(errors::invalid("the method takes no arguments"));
    }
    let result = match method {
        Method::Claim => claim(service, gateway, operation, decode(arguments)?).await?.encode_to_vec(),
        Method::Activate => activate(service, gateway, operation).await?.encode_to_vec(),
        Method::Withdraw => withdraw(service, gateway, operation, None).await?.encode_to_vec(),
        Method::AbandonMove => {
            let AbandonMoveArguments { reason } = decode(arguments)?;
            withdraw(service, gateway, operation, Some(reason)).await?.encode_to_vec()
        }
        Method::Depart => depart(service, gateway, operation).await?.encode_to_vec(),
    };
    Ok(result)
}

/// Claims a login's session, or the destination of the move control queued under `operation`.
async fn claim(
    service: &SyncService,
    gateway: &str,
    operation: &str,
    arguments: ClaimArguments,
) -> Result<ClaimResult, Error> {
    let request = match (arguments.login, held(service, gateway, operation)?) {
        (Some(_), Some(stored)) if stored.request.source.is_some() => {
            return Err(errors::error(Code::OperationMismatch, "the operation ID names a move"));
        }
        (Some(login), _) => login_request(gateway, operation, login),
        (None, Some(stored)) if stored.request.source.is_some() => stored.request,
        (None, Some(_)) => return Err(errors::error(Code::OperationMismatch, "the operation ID names a login")),
        (None, None) => return Err(errors::invalid("no move is queued under this operation ID")),
    };
    let control = service.control.clone();
    let outcome = match service.operations.admit(async move { control.claim(request).await }).await {
        Ok(assignment) => Outcome::Assignment(assigned(assignment)?),
        Err(Failure::Unresolved(ROUTE_AGAIN)) => Outcome::Refusal(ClaimRefusal::RouteAgain.into()),
        Err(Failure::Invalid(ALREADY_OWNED)) => Outcome::Refusal(ClaimRefusal::AlreadyConnected.into()),
        Err(failure) => return Err(errors::operation(&failure)),
    };
    Ok(ClaimResult { outcome: Some(outcome) })
}

async fn activate(service: &SyncService, gateway: &str, operation: &str) -> Result<ActivateResult, Error> {
    let identity = held(service, gateway, operation)?.and_then(|stored| stored.identity);
    let identity = identity.ok_or_else(|| errors::invalid("no claim under this operation ID"))?;
    let control = service.control.clone();
    let activation = control::ActivateClaim { claim: Some(identity) };
    match service.operations.admit(async move { control.activate(activation).await }).await {
        Ok(_) => Ok(ActivateResult { waiting: false }),
        Err(Failure::Unresolved(ROSTER_WAITING)) => Ok(ActivateResult { waiting: true }),
        Err(failure) => Err(errors::operation(&failure)),
    }
}

/// Withdraws the claim or queued move, or with a `reason`, records why its move was abandoned and leaves the
/// withdrawal to control.
async fn withdraw(
    service: &SyncService,
    gateway: &str,
    operation: &str,
    reason: Option<String>,
) -> Result<WithdrawResult, Error> {
    let Some(stored) = held(service, gateway, operation)? else {
        return Ok(WithdrawResult { unknown: true });
    };
    let control = service.control.clone();
    let withdrawn = service.operations.admit(async move {
        match reason {
            Some(reason) => {
                control.abandon_move(control::AbandonMoveRequest { claim: Some(stored.request), reason }).await
            }
            None => control.cancel(stored.request).await,
        }
    });
    withdrawn.await.map_err(|failure| errors::operation(&failure))?;
    Ok(WithdrawResult { unknown: false })
}

/// Withdraws the claim after its player's connection ended and reports whether their membership ended with it.
async fn depart(service: &SyncService, gateway: &str, operation: &str) -> Result<DepartResult, Error> {
    let stored = held(service, gateway, operation)?;
    let stored = stored.ok_or_else(|| errors::invalid("no claim under this operation ID"))?;
    let control = service.control.clone();
    let status = service.operations.admit(async move { control.reconcile_departure(stored.request).await });
    let status = status.await.map_err(|failure| errors::operation(&failure))?;
    Ok(DepartResult { departed: status.departed })
}

/// The claim or queued move stored under `operation`, which must be `gateway`'s.
fn held(service: &SyncService, gateway: &str, operation: &str) -> Result<Option<StoredClaim>, Error> {
    let stored = service.control.stored_claim(operation).map_err(|failure| errors::operation(&failure))?;
    if stored.as_ref().is_some_and(|stored| stored.request.proxy_id != gateway) {
        return Err(errors::denied("the claim belongs to another gateway"));
    }
    Ok(stored)
}

fn login_request(gateway: &str, operation: &str, login: GatewayLogin) -> control::ClaimRequest {
    let identity = login.player.map(|player| control::Identity {
        uuid: player.uuid,
        username: player.username,
        properties: player
            .properties
            .into_iter()
            .map(|property| control::Property {
                name: property.name,
                value: property.value,
                signature: property.signature,
            })
            .collect(),
    });
    let demand = login.demand.map(|demand| control::SessionDemand {
        key: demand.key,
        session_type: demand.session_type,
        machine_profile: demand.machine_profile,
    });
    control::ClaimRequest {
        operation_id: operation.to_owned(),
        proxy_id: gateway.to_owned(),
        connection_id: login.connection_id,
        identity,
        demand,
        source: None,
        deployment: login.deployment,
    }
}

fn assigned(assignment: control::Assignment) -> Result<ClaimAssignment, Error> {
    let incomplete = || errors::error(Code::Unavailable, "control returned an incomplete assignment");
    let claim = assignment.claim.ok_or_else(incomplete)?;
    let session = assignment.delivery.and_then(|delivery| delivery.session).ok_or_else(incomplete)?;
    let configuration = assignment.configuration.ok_or_else(incomplete)?;
    let preparation = assignment.preparation.ok_or_else(incomplete)?;
    let generation = Generation::from_wire(claim.delivery_generation);
    Ok(ClaimAssignment {
        generation: Some(Position { epoch: generation.epoch, revision: generation.revision }),
        session: session.id,
        protocol: configuration.protocol,
        endpoint: preparation.endpoint,
        capability: preparation.capability,
    })
}
