use std::sync::Arc;

use chunk_contract::Schema;
use chunk_js::{DeploymentId, Json};
use chunk_proto::v1::{CommandCatalog, CommandScope, CommandSuggestionRequest, CommandSuggestionResult};
use sha2::{Digest, Sha256};
use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::{
    ActionHandle, ActionId, ActionStatus, Backend, Call, Error, Result,
    service::{Command, Request},
};

pub(crate) mod effects;
mod transport;
pub use transport::CommandService;

/// Platform effects a command may have pending at once.
const PENDING_EFFECTS: usize = 8;

#[derive(Clone)]
pub(crate) enum Purpose {
    Function,
    Hook,
    Command(Arc<CommandBinding>),
}

impl Purpose {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Hook => "hook",
            Self::Command(_) => "command",
        }
    }
    pub fn command(self) -> Option<Arc<CommandBinding>> {
        if let Self::Command(binding) = self { Some(binding) } else { None }
    }
    /// The digest of who started a command, which only they may resolve it by.
    pub fn owner(&self) -> Option<[u8; 32]> {
        if let Self::Command(binding) = self { binding.owner } else { None }
    }
    /// Bytes the command's binding retains while it runs.
    pub fn bytes(&self) -> usize {
        if let Self::Command(binding) = self { binding.bytes() } else { 0 }
    }
}

/// Bytes a command scope retains, charged at admission.
pub(crate) fn scope_bytes(scope: &CommandScope) -> usize {
    [
        &scope.proxy_id,
        &scope.player_uuid,
        &scope.username,
        &scope.session_id,
        &scope.app,
        &scope.session_type,
        &scope.domain,
        &scope.scope_id,
        &scope.connection_id,
        &scope.claim_operation_id,
    ]
    .iter()
    .map(|value| value.len())
    .sum()
}

#[derive(Clone)]
pub(crate) struct CommandBinding {
    pub scope: CommandScope,
    pub input: String,
    pub effects: mpsc::Sender<PlatformEffect>,
    /// The digest of the credential that started the command, if [`Backend::start_command`] did.
    pub owner: Option<[u8; 32]>,
}

impl CommandBinding {
    pub fn bytes(&self) -> usize {
        scope_bytes(&self.scope) + self.input.len()
    }
}

/// What an identity from [`Backend::allocate_action_id`] names to a command's owner, resolved without consulting any
/// deployment.
#[derive(Clone, Debug)]
pub enum CommandIdentity {
    /// Prepared, and nothing started under it yet.
    Unused,
    /// A command the owner started, running or with its outcome retained.
    Started(ActionStatus),
    /// A command another owner started.
    Foreign,
    /// An action or hook.
    Other,
}

pub(crate) fn owner_digest(owner: &str) -> [u8; 32] {
    Sha256::digest(owner.as_bytes()).into()
}

pub(crate) struct PlatformEffect {
    pub sequence: u32,
    pub request: Json,
    pub result: Schema,
    pub receipt: bool,
    pub reply: Request<Arc<str>>,
}

#[derive(Clone)]
pub(crate) struct Prepared {
    pub deployment: DeploymentId,
    pub scope: CommandScope,
    pub command: String,
    pub input: String,
    pub follow_player: bool,
}

impl Prepared {
    pub fn call(&self) -> Call {
        Call {
            deployment: self.deployment.clone(),
            function: self.command.clone(),
            arguments: serde_json::Value::Null.into(),
            caller: serde_json::Value::Null.into(),
        }
    }
}

/// The operation ID effect `sequence` of command invocation `invocation` names in its receipt.
pub(crate) fn effect_operation(invocation: &str, sequence: u32) -> String {
    format!("action/{invocation}/platform/{sequence}")
}

/// Checks the `result` JSON an effect's performer returned, `None` if the effect failed, against what the command
/// expects of it.
pub(crate) fn effect_result(invocation: &str, effect: &PlatformEffect, result: Option<&[u8]>) -> Result<Arc<str>> {
    let result = result.ok_or(Error::Invalid("command effect failed; earlier effects may have completed"))?;
    if result.len() > 64 * 1024 {
        return Err(Error::Invalid("command effect result limit"));
    }
    let mut value = serde_json::from_slice(result)?;
    effect.result.normalize_api(&mut value);
    chunk_contract::validate_wire_value(&value).map_err(Error::Invalid)?;
    if !effect.result.accepts(&value)
        || (effect.receipt && value["operationId"] != effect_operation(invocation, effect.sequence))
    {
        return Err(Error::Contract);
    }
    Ok(serde_json::to_string(&value)?.into())
}

/// A platform effect a running command asked for, validated against its scope, which its runner performs and then
/// finishes. Dropping it fails the effect.
pub struct CommandEffect {
    effect: PlatformEffect,
    invocation: String,
}

impl CommandEffect {
    /// Numbers the command's effects; each is unique to its invocation.
    #[must_use]
    pub fn sequence(&self) -> u32 {
        self.effect.sequence
    }

    /// The effect as a `chunk_contract::Effect` in JSON.
    #[must_use]
    pub fn request(&self) -> &Json {
        &self.effect.request
    }

    /// The operation ID unique to this effect, which its receipt names.
    #[must_use]
    pub fn operation_id(&self) -> String {
        effect_operation(&self.invocation, self.effect.sequence)
    }

    /// Whether the command stopped waiting for the effect.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.effect.reply.cancellation.is_cancelled()
    }

    /// Finishes an effect whose result is a receipt of its acceptance.
    pub fn accept(self) {
        let receipt = serde_json::json!({"state": "accepted", "operationId": self.operation_id()});
        self.finish(Some(receipt.to_string().as_bytes()));
    }

    /// Finishes an effect that runs on after its receipt of acceptance, returning the admission it holds, which the
    /// performer keeps until the effect's work ends.
    pub fn accept_running(self) -> OwnedSemaphorePermit {
        let receipt = serde_json::json!({"state": "accepted", "operationId": self.operation_id()});
        let result = effect_result(&self.invocation, &self.effect, Some(receipt.to_string().as_bytes()));
        self.effect.reply.finish_holding(result)
    }

    /// Finishes the effect with its `result` JSON, or `None` if it failed.
    pub fn finish(self, result: Option<&[u8]>) {
        let result = effect_result(&self.invocation, &self.effect, result);
        self.effect.reply.finish(result);
    }
}

/// The platform effects a command started with [`Backend::start_command`] asks for.
pub struct CommandEffects {
    receiver: mpsc::Receiver<PlatformEffect>,
    invocation: String,
}

impl CommandEffects {
    /// The next effect, or `None` once no more can come through this receiver, as for a retry that joined a command
    /// already started.
    pub async fn recv(&mut self) -> Option<CommandEffect> {
        let effect = self.receiver.recv().await?;
        Some(CommandEffect { effect, invocation: self.invocation.clone() })
    }
}

impl Backend {
    /// The commands `scope` sees in deployment `id`, and which of them its permission queries allow.
    /// # Errors
    /// Rejects an invalid scope, an unknown deployment and failed permission queries.
    pub async fn command_catalog(&self, id: DeploymentId, scope: CommandScope) -> Result<CommandCatalog> {
        let bytes = scope_bytes(&scope);
        self.submit_sized(bytes, |reply| Command::Catalog { id, scope, reply }).await
    }

    /// The values the suggestion query `request` names offers for its input.
    /// # Errors
    /// Rejects an invalid scope or input, a query the command doesn't declare, and a failed query.
    pub async fn command_suggestions(
        &self,
        id: DeploymentId,
        request: CommandSuggestionRequest,
    ) -> Result<CommandSuggestionResult> {
        let bytes = request.scope.as_ref().map_or(0, scope_bytes)
            + request.command_id.len()
            + request.query.len()
            + request.input.len();
        self.submit_sized(bytes, |reply| Command::Suggest { id, request, reply }).await
    }

    /// Resolves `id` for `owner` without consulting any deployment.
    /// # Errors
    /// Reports an identity this backend didn't allocate, or whose outcome is gone, as unknown.
    pub async fn command_identity(&self, id: ActionId, owner: &str) -> Result<CommandIdentity> {
        let owner = owner_digest(owner);
        self.submit_sized(id.incarnation.len(), |reply| Command::OwnedIdentity { id, owner, reply }).await
    }

    /// Starts `command` with `input` for `scope` in `deployment` under an identity from
    /// [`Self::allocate_action_id`], through the same admission as [`Self::start_action`], and retains its outcome for
    /// `owner`, whom [`Self::command_identity`] resolves it for. `caller` is the handler's `ctx.caller`.
    /// # Errors
    /// Rejects identities this backend didn't allocate, mismatched requests, commands the scope may not run, invalid
    /// input and exhausted capacity.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_command(
        &self,
        id: ActionId,
        owner: &str,
        deployment: DeploymentId,
        scope: CommandScope,
        command: String,
        input: String,
        caller: Json,
    ) -> Result<(ActionHandle, CommandEffects)> {
        let invocation = id.to_string();
        let (effects, receiver) = mpsc::channel(PENDING_EFFECTS);
        let call =
            Call { deployment, function: command, arguments: serde_json::json!({"input": input}).into(), caller };
        let bytes = id.incarnation.len() + call.bytes() + scope_bytes(&scope) + input.len();
        let owner = Some(owner_digest(owner));
        let purpose = Purpose::Command(Arc::new(CommandBinding { scope, input, effects, owner }));
        let handle =
            self.submit_sized(bytes, |reply| Command::StartAction { id, call, purpose, retain: true, reply }).await?;
        Ok((handle, CommandEffects { receiver, invocation }))
    }
}

#[cfg(test)]
mod tests;
