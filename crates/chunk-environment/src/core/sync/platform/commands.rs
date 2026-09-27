//! A gateway's commands: `chunk:commands`, `chunk:suggest`, `chunk:command` and `chunk:effect`. Core derives each
//! command's scope from its player's arrived claim, and a command runs through the backend's action admission.

mod effects;

use super::{
    super::{SyncService, app, errors},
    decode,
};
use chunk_contract::DomainManifest;
use chunk_control::ArrivedClaim;
use chunk_js::DeploymentId;
use chunk_proto::{
    sync::v1::{
        CallRequest, CommandArguments, CommandResult, CommandsResult, EffectArguments, EffectResult, Error,
        SuggestArguments, SuggestResult, error::Code,
    },
    v1::{CommandScope, CommandSuggestionRequest},
};
use prost::Message;

#[derive(Clone, Copy)]
pub(super) enum Method {
    Commands,
    Suggest,
    Command,
    Effect,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "commands" => Self::Commands,
            "suggest" => Self::Suggest,
            "command" => Self::Command,
            "effect" => Self::Effect,
            _ => return None,
        })
    }
}

/// Runs `method` for `gateway`, returning its encoded result.
pub(super) async fn call(
    service: &SyncService,
    gateway: &str,
    method: Method,
    request: &CallRequest,
) -> Result<Vec<u8>, Error> {
    let backend = service.app.backend();
    let result = match method {
        Method::Commands => {
            if !request.arguments.is_empty() {
                return Err(errors::invalid("chunk:commands takes no arguments"));
            }
            app::reject_prepared(&request.operation_id)?;
            let Origin { deployment, scope, .. } = origin(service, gateway, request).await?;
            let catalog =
                backend.command_catalog(deployment, scope).await.map_err(|failure| errors::backend(&failure))?;
            CommandsResult { commands_json: catalog.commands_json, allowed: catalog.allowed_ids }.encode_to_vec()
        }
        Method::Suggest => {
            let SuggestArguments { command_id, query, input, cursor } = decode(&request.arguments)?;
            app::reject_prepared(&request.operation_id)?;
            let Origin { deployment, scope, .. } = origin(service, gateway, request).await?;
            let request = CommandSuggestionRequest { scope: Some(scope), command_id, query, input, cursor };
            let suggestions = backend.command_suggestions(deployment, request).await;
            SuggestResult { values: suggestions.map_err(|failure| errors::backend(&failure))?.values }.encode_to_vec()
        }
        Method::Command => run(service, gateway, request).await?.encode_to_vec(),
        Method::Effect => {
            if request.caller.is_some() || !request.operation_id.is_empty() {
                return Err(errors::invalid("chunk:effect takes no caller or operation ID"));
            }
            let EffectArguments { operation_id, sequence, failed } = decode(&request.arguments)?;
            let run = service.runs.get(&operation_id);
            if run.as_ref().is_some_and(|run| run.gateway != gateway) {
                return Err(errors::denied("another gateway runs this command"));
            }
            let acknowledged = run.is_some_and(|run| run.acknowledge(sequence, failed));
            EffectResult { unknown: !acknowledged }.encode_to_vec()
        }
    };
    Ok(result)
}

/// Starts the command `request` names under its prepared operation ID, or joins it, and returns its outcome. The run
/// outlives a dropped call, so a retry finds its outcome.
async fn run(service: &SyncService, gateway: &str, request: &CallRequest) -> Result<CommandResult, Error> {
    let id = app::prepared(&request.operation_id)?;
    let CommandArguments { command_id, input } = decode(&request.arguments)?;
    let Origin { claim, deployment, scope, manifest } = origin(service, gateway, request).await?;
    let follow = manifest.commands.get(&command_id).is_some_and(|command| command.follow_player);
    let run = service.runs.open(&request.operation_id, gateway)?;
    let caller = serde_json::json!({"kind": "gateway", "player": scope.player_uuid}).into();
    let performer = effects::Performer {
        control: service.control.clone(),
        stop: service.stop.clone(),
        gateway: gateway.to_owned(),
        player: scope.player_uuid.clone(),
        origin: claim,
        follow,
        run,
    };
    let backend = service.app.backend().clone();
    let task = tokio::spawn(async move {
        let (handle, effects) = backend.start_command(id, deployment, scope, command_id, input, caller).await?;
        performer.drive(handle, effects).await
    });
    let outcome = task.await.map_err(|_| errors::error(Code::OutcomeUnknown, "the command's task failed"))?;
    let json = outcome.map_err(|failure| errors::backend(&failure))?;
    Ok(CommandResult { result_json: json.as_bytes().to_vec() })
}

/// Where a command runs: its player's arrived claim, the deployment and domain manifest of the claim's session, and
/// the scope the backend checks the command against.
struct Origin {
    claim: ArrivedClaim,
    deployment: DeploymentId,
    scope: CommandScope,
    manifest: DomainManifest,
}

/// The origin of a command for the player `request`'s caller names, whose arrived claim `gateway` must hold.
async fn origin(service: &SyncService, gateway: &str, request: &CallRequest) -> Result<Origin, Error> {
    let caller = request.caller.as_ref().filter(|caller| caller.session.is_empty() && !caller.player.is_empty());
    let player = &caller.ok_or_else(|| errors::invalid("a command method names the player it runs for"))?.player;
    let claim = service.control.arrived_claim(gateway, player).map_err(|failure| errors::control(&failure))?;
    let deployment = DeploymentId::new(&claim.scope.deployment).map_err(|_| errors::invalid("invalid deployment"))?;
    let manifest = service.app.backend().domain_manifest(deployment.clone()).await;
    let manifest = manifest.map_err(|failure| errors::backend(&failure))?;
    let manifest = manifest.ok_or_else(|| errors::error(Code::Contract, "the deployment declares no commands"))?;
    let domain = manifest.apps.get(&claim.scope.app).cloned();
    let domain = domain.ok_or_else(|| errors::error(Code::Contract, "the session's app belongs to no domain"))?;
    let identity = &claim.identity;
    let scope = CommandScope {
        proxy_id: gateway.to_owned(),
        player_uuid: player.clone(),
        username: claim.request.identity.as_ref().map(|identity| identity.username.clone()).unwrap_or_default(),
        session_id: claim.session.clone(),
        app: claim.scope.app.clone(),
        session_type: claim.session_type.clone(),
        domain,
        scope_id: identity.operation_id.clone(),
        connection_id: claim.request.connection_id.clone(),
        claim_operation_id: identity.operation_id.clone(),
        membership_generation: identity.membership_generation,
        delivery_generation: identity.delivery_generation,
    };
    Ok(Origin { claim, deployment, scope, manifest })
}
