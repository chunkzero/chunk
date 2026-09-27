//! A gateway's commands: `chunk:commands`, `chunk:suggest`, `chunk:command` and `chunk:effect`. Core derives a new
//! command's scope from its player's arrived claim, and the command runs through the backend's action admission, then
//! on until it finishes or nothing follows its `command/<op>` topic. Permission and suggestion queries, and the
//! command's handler, see the gateway caller of its player. Each call's request is charged against the backend's
//! request memory before it waits for anything, until what it holds drops or the backend takes the charge over.

mod effects;

use super::{
    super::{
        SyncService, app, errors,
        runs::{Run, Runs, closed_before_start, outcome},
    },
    decode,
};
use chunk_backend::{
    ActionHandle, ActionId, ActionStatus, Backend, CommandEffects, CommandIdentity, CommandRequest, RequestCharge,
};
use chunk_contract::DomainManifest;
use chunk_control::{ArrivedClaim, Control};
use chunk_js::{DeploymentId, Json};
use chunk_proto::{
    sync::v1::{
        CallRequest, CommandArguments, CommandStarted, CommandsResult, EffectArguments, EffectResult, Error,
        SuggestArguments, SuggestResult, error::Code,
    },
    v1::{CommandScope, CommandSuggestionRequest},
};
use prost::Message;
use std::sync::Arc;

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

/// Runs `method` for `gateway`, which presents `credential`, returning its encoded result.
pub(super) async fn call(
    service: &SyncService,
    gateway: &str,
    credential: &str,
    method: Method,
    request: CallRequest,
) -> Result<Vec<u8>, Error> {
    let (control, backend) = (&service.control, service.app.backend());
    let charge = backend.charge_request(request.encoded_len()).map_err(|failure| errors::backend(&failure))?;
    let result = match method {
        Method::Commands => {
            if !request.arguments.is_empty() {
                return Err(errors::invalid("chunk:commands takes no arguments"));
            }
            app::reject_reserved(&request.operation_id)?;
            let player = player(&request)?;
            let Origin { deployment, scope, .. } = origin(control, backend, gateway, player).await?;
            let catalog = backend.command_catalog(deployment, scope, Some(caller(player))).await;
            let catalog = catalog.map_err(|failure| errors::backend(&failure))?;
            CommandsResult { commands_json: catalog.commands_json, allowed: catalog.allowed_ids }.encode_to_vec()
        }
        Method::Suggest => {
            let SuggestArguments { command_id, query, input, cursor } = decode(&request.arguments)?;
            app::reject_reserved(&request.operation_id)?;
            let player = player(&request)?.to_owned();
            drop(request);
            let Origin { deployment, scope, .. } = origin(control, backend, gateway, &player).await?;
            let suggestion = CommandSuggestionRequest { scope: Some(scope), command_id, query, input, cursor };
            let suggestions = backend.command_suggestions(deployment, charge, suggestion, Some(caller(&player))).await;
            SuggestResult { values: suggestions.map_err(|failure| errors::backend(&failure))?.values }.encode_to_vec()
        }
        Method::Command => start(service, gateway, credential, request, charge).await?.encode_to_vec(),
        Method::Effect => {
            if request.caller.is_some() || !request.operation_id.is_empty() {
                return Err(errors::invalid("chunk:effect takes no caller or operation ID"));
            }
            let EffectArguments { operation_id, sequence, failed } = decode(&request.arguments)?;
            app::prepared(&operation_id)?;
            let run = service.runs.get(&operation_id);
            let acknowledged = match run {
                Some(run) => run.acknowledge(credential, sequence, failed)?,
                None => false,
            };
            EffectResult { unknown: !acknowledged }.encode_to_vec()
        }
    };
    Ok(result)
}

/// Starts the command `request` names under its prepared operation ID for `credential`, or finds the one started
/// there for the same request, and returns once the backend admitted it. `charge` covers what the request holds until
/// the backend takes it over.
async fn start(
    service: &SyncService,
    gateway: &str,
    credential: &str,
    request: CallRequest,
    charge: RequestCharge,
) -> Result<CommandStarted, Error> {
    let id = app::prepared(&request.operation_id)?;
    let CommandArguments { command_id, input } = decode(&request.arguments)?;
    let player = player(&request)?.to_owned();
    let operation = request.operation_id.clone();
    drop(request);
    let fingerprint = CommandRequest::new(&command_id, &input, &player);
    let (run, new) = service.runs.begin(&operation, credential, fingerprint, service.stop.child_token())?;
    run.permits(credential, Some(fingerprint))?;
    if !new {
        // A duplicate waits holding only the run, and a rejected start leaves the ID for the gateway to retry.
        drop((command_id, input, player, charge));
        if !run.started().await? {
            return Err(errors::error(Code::Unavailable, "retry the command"));
        }
        return Ok(CommandStarted {});
    }
    let starting = Starting {
        control: service.control.clone(),
        backend: service.app.backend().clone(),
        runs: service.runs.clone(),
        run,
        operation,
        gateway: gateway.to_owned(),
        credential: credential.to_owned(),
        player,
        fingerprint,
    };
    // The start outlives a dropped call, so a retry finds the command it admitted.
    let started = tokio::spawn(starting.start(id, charge, command_id, input)).await;
    started.map_err(|_| errors::error(Code::OutcomeUnknown, "the command's task failed"))??;
    Ok(CommandStarted {})
}

/// A command starting under `operation`, which holds its place in the runs until it starts or is rejected.
struct Starting {
    control: Arc<Control>,
    backend: Backend,
    runs: Arc<Runs>,
    run: Arc<Run>,
    operation: String,
    gateway: String,
    credential: String,
    player: String,
    fingerprint: CommandRequest,
}

enum Admitted {
    Started {
        origin: Box<ArrivedClaim>,
        follow: bool,
        handle: ActionHandle,
        effects: CommandEffects,
    },
    /// The command already ran, and the backend retains its outcome.
    Retained(chunk_backend::Result<Arc<str>>),
}

impl Starting {
    /// Resolves `id` before anything else, starting the command under it if it's unused, then drives it until it
    /// finishes.
    async fn start(self, id: ActionId, charge: RequestCharge, command: String, input: String) -> Result<(), Error> {
        let admitted = self.admit(id, charge, command, input).await;
        let Self { control, backend, runs, run, operation, gateway, player, .. } = self;
        match admitted {
            Ok(Admitted::Started { origin, follow, handle, effects }) => {
                run.start();
                let origin = *origin;
                let performer = effects::Performer {
                    control,
                    backend: backend.clone(),
                    gateway,
                    player,
                    origin,
                    follow,
                    run: run.clone(),
                };
                tokio::spawn(async move {
                    let result = performer.drive(handle, effects).await;
                    run.finish(outcome(&backend, result));
                    runs.settle(&operation, &run);
                });
            }
            Ok(Admitted::Retained(result)) => {
                run.finish(outcome(&backend, result));
                runs.settle(&operation, &run);
            }
            Err(error) => {
                runs.reject(&operation, &run);
                return Err(error);
            }
        }
        Ok(())
    }

    /// Admits the command unless it's cancelled first, as when core stops, so a cancelled command never starts.
    async fn admit(
        &self,
        id: ActionId,
        charge: RequestCharge,
        command: String,
        input: String,
    ) -> Result<Admitted, Error> {
        let failed = |failure: chunk_backend::Error| errors::backend(&failure);
        let identity = self.backend.command_identity(id.clone(), &self.credential, Some(self.fingerprint));
        match self.unless_cancelled(identity).await?.map_err(failed)? {
            CommandIdentity::Unused => {}
            CommandIdentity::Started(ActionStatus::Finished(result)) => return Ok(Admitted::Retained(result)),
            CommandIdentity::Started(ActionStatus::Running) => {
                return Err(errors::error(Code::Unavailable, "retry the command"));
            }
            CommandIdentity::Foreign => return Err(errors::denied("another gateway ran this command")),
            CommandIdentity::Other => {
                return Err(errors::error(Code::OperationMismatch, "the operation ID ran another request"));
            }
        }
        let origin = origin(&self.control, &self.backend, &self.gateway, &self.player);
        let Origin { claim, deployment, scope, manifest } = self.unless_cancelled(origin).await??;
        let follow = manifest.commands.get(&command).is_some_and(|command| command.follow_player);
        drop(manifest);
        let (credential, caller) = (&self.credential, caller(&self.player));
        let started = self.backend.start_command(
            id,
            charge,
            credential,
            self.fingerprint,
            deployment,
            scope,
            command,
            input,
            caller,
        );
        let (handle, effects) = self.unless_cancelled(started).await?.map_err(failed)?;
        if self.run.token().is_cancelled() {
            handle.cancel();
        }
        Ok(Admitted::Started { origin: Box::new(claim), follow, handle, effects })
    }

    /// Waits for `work`, which is dropped once the command is cancelled first: by its subscription closing, or core
    /// stopping.
    async fn unless_cancelled<T>(&self, work: impl Future<Output = T>) -> Result<T, Error> {
        let cancel = self.run.token();
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(if self.run.closed() {
                closed_before_start()
            } else {
                errors::error(Code::Unavailable, "core is stopping")
            }),
            output = work => Ok(output),
        }
    }
}

/// The player `request`'s caller names.
fn player(request: &CallRequest) -> Result<&str, Error> {
    let caller = request.caller.as_ref().filter(|caller| caller.session.is_empty() && !caller.player.is_empty());
    Ok(&caller.ok_or_else(|| errors::invalid("a command method names the player it runs for"))?.player)
}

/// The caller a command for `player` runs as.
fn caller(player: &str) -> Json {
    serde_json::json!({"kind": "gateway", "player": player}).into()
}

/// Where a command runs: its player's arrived claim, the deployment and domain manifest of the claim's session, and
/// the scope the backend checks the command against.
struct Origin {
    claim: ArrivedClaim,
    deployment: DeploymentId,
    scope: CommandScope,
    manifest: DomainManifest,
}

/// The origin of a command for `player`, whose arrived claim `gateway` must hold.
async fn origin(control: &Control, backend: &Backend, gateway: &str, player: &str) -> Result<Origin, Error> {
    let claim = control.arrived_claim(gateway, player).map_err(|failure| errors::control(&failure))?;
    let deployment = DeploymentId::new(&claim.scope.deployment).map_err(|_| errors::invalid("invalid deployment"))?;
    let manifest = backend.domain_manifest(deployment.clone()).await;
    let manifest = manifest.map_err(|failure| errors::backend(&failure))?;
    let manifest = manifest.ok_or_else(|| errors::error(Code::Contract, "the deployment declares no commands"))?;
    let domain = manifest.apps.get(&claim.scope.app).cloned();
    let domain = domain.ok_or_else(|| errors::error(Code::Contract, "the session's app belongs to no domain"))?;
    let identity = &claim.identity;
    let scope = CommandScope {
        proxy_id: gateway.to_owned(),
        player_uuid: player.to_owned(),
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
