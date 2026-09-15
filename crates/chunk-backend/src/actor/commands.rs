use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use chunk_contract::{
    CommandSuggestions, Deployment, Function, FunctionKind, Schema, Visibility, validate_wire_value, visible_commands,
};
use chunk_js::{Cancellation, DeploymentId, Json, Mode};
use chunk_proto::v1::{CommandCatalog, CommandScope, CommandSuggestionRequest, CommandSuggestionResult};
use serde_json::{Value, json};

use super::Actor;
use crate::{
    Call, Error, Result,
    commands::{CommandBinding, Prepared},
};

impl Actor {
    fn command_deployment(&self, id: &DeploymentId) -> Result<Arc<Deployment>> {
        self.check_deployment(id)?;
        self.versions.get(id).and_then(Option::as_ref).cloned().ok_or(Error::Unknown)
    }

    fn command_query(
        &mut self,
        deployment: &DeploymentId,
        caller: &Json,
        function: String,
        arguments: Value,
        cancellation: &Cancellation,
    ) -> Result<Value> {
        // Never authorize effects using permission writes that may still roll back.
        if !self.pending.is_empty() || self.recovering {
            return Err(Error::Busy);
        }
        let mut call =
            Call { deployment: deployment.clone(), function, arguments: arguments.into(), caller: caller.clone() };
        self.normalize_scoped_call(&mut call, true)?;
        let (execution, _) = self.evaluate(&call, Mode::Query, self.view.clone(), cancellation)?;
        Ok(serde_json::from_str(&execution.value)?)
    }

    pub(super) fn command_permission(
        &mut self,
        deployment: &Deployment,
        scope: &CommandScope,
        command: &str,
        cancellation: &Cancellation,
    ) -> Result<Json> {
        let caller = scope_caller(deployment, scope)?;
        let descriptor = selected(deployment, scope, command)?;
        if let Some(query) = &descriptor.permission {
            let id = DeploymentId::new(&deployment.id)?;
            if self.command_query(&id, &caller, query.clone(), json!({}), cancellation)? != Value::Bool(true) {
                return Err(Error::Unknown);
            }
        }
        Ok(caller)
    }

    pub(super) fn command_catalog(
        &mut self,
        id: &DeploymentId,
        scope: &CommandScope,
        cancellation: &Cancellation,
    ) -> Result<CommandCatalog> {
        let deployment = self.command_deployment(id)?;
        scope_caller(&deployment, scope)?;
        let manifest = deployment.domains.as_ref().ok_or(Error::Unknown)?;
        let ids: BTreeSet<_> = visible_commands(&manifest.commands, &scope.domain, &[])
            .map_err(Error::Invalid)?
            .values()
            .copied()
            .collect();
        let mut commands = BTreeMap::new();
        let mut allowed_ids = Vec::new();
        for id in ids {
            commands.insert(id, &manifest.commands[id]);
            match self.command_permission(&deployment, scope, id, cancellation) {
                Ok(_) => allowed_ids.push(id.into()),
                Err(Error::Unknown) => {}
                Err(error) => return Err(error),
            }
        }
        let commands_json = serde_json::to_vec(&commands)?;
        if commands_json.len() > 1024 * 1024 {
            return Err(Error::Busy);
        }
        Ok(CommandCatalog { commands_json, allowed_ids })
    }

    pub(super) fn prepare_command(
        &mut self,
        id: DeploymentId,
        scope: CommandScope,
        command: String,
        input: String,
        cancellation: &Cancellation,
    ) -> Result<Prepared> {
        let deployment = self.command_deployment(&id)?;
        self.command_permission(&deployment, &scope, &command, cancellation)?;
        let descriptor = selected(&deployment, &scope, &command)?;
        descriptor.parse(&input).map_err(Error::Invalid)?;
        Ok(Prepared { deployment: id, scope, command, input, follow_player: descriptor.follow_player })
    }

    pub(super) fn resolve_command(
        &mut self,
        deployment: &Deployment,
        call: &mut Call,
        binding: &CommandBinding,
        cancellation: &Cancellation,
    ) -> Result<Function> {
        call.caller = self.command_permission(deployment, &binding.scope, &call.function, cancellation)?;
        let descriptor = selected(deployment, &binding.scope, &call.function)?;
        let parsed = descriptor.parse(&binding.input).map_err(Error::Invalid)?;
        call.arguments=json!({"route":parsed.route,"arguments":parsed.arguments,"player":{"uuid":binding.scope.player_uuid,"username":binding.scope.username}}).into();
        Ok(Function {
            kind: FunctionKind::Action,
            visibility: Visibility::Internal,
            export: descriptor.export.clone(),
            arguments: Schema::Null,
            result: Schema::Null,
        })
    }

    pub(super) fn command_suggest(
        &mut self,
        id: &DeploymentId,
        request: CommandSuggestionRequest,
        cancellation: &Cancellation,
    ) -> Result<CommandSuggestionResult> {
        let scope = request.scope.ok_or(Error::Invalid("missing command scope"))?;
        let deployment = self.command_deployment(id)?;
        let caller = self.command_permission(&deployment, &scope, &request.command_id, cancellation)?;
        let descriptor = selected(&deployment, &scope, &request.command_id)?;
        if request.input.len() > 3 * (chunk_contract::MAX_COMMAND_INPUT + 1)
            || request.input.strip_prefix('/').unwrap_or(&request.input).encode_utf16().count()
                > chunk_contract::MAX_COMMAND_INPUT
            || request.input.chars().any(char::is_control)
            || usize::try_from(request.cursor).unwrap_or(usize::MAX) > request.input.encode_utf16().count()
        {
            return Err(Error::Invalid("suggestion input"));
        }
        let declared=descriptor.routes.iter().flat_map(|route|&route.arguments).any(|argument|matches!(&argument.suggestions,Some(CommandSuggestions::Query(query)) if query.query==request.query));
        if !declared {
            return Err(Error::Unknown);
        }
        let result = self.command_query(
            id,
            &caller,
            request.query,
            json!({"input":request.input,"cursor":request.cursor}),
            cancellation,
        )?;
        let values: Vec<String> = serde_json::from_value(result)?;
        if values.len() > 64
            || values.iter().any(|value| {
                value.is_empty()
                    || value.len() > 1024
                    || value.encode_utf16().count() > 256
                    || value.chars().any(char::is_control)
            })
        {
            return Err(Error::Invalid("suggestion result limit"));
        }
        Ok(CommandSuggestionResult { values })
    }
}

fn selected<'a>(deployment: &'a Deployment, scope: &CommandScope, id: &str) -> Result<&'a chunk_contract::Command> {
    let manifest = deployment.domains.as_ref().ok_or(Error::Unknown)?;
    let command = manifest.commands.get(id).ok_or(Error::Unknown)?;
    if !visible_commands(&manifest.commands, &scope.domain, &[])
        .map_err(Error::Invalid)?
        .values()
        .any(|candidate| *candidate == id)
    {
        return Err(Error::Unknown);
    }
    Ok(command)
}

fn scope_caller(deployment: &Deployment, scope: &CommandScope) -> Result<Json> {
    let text = |value: &str| !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control);
    for value in [
        &scope.proxy_id,
        &scope.player_uuid,
        &scope.username,
        &scope.session_id,
        &scope.app,
        &scope.session_type,
        &scope.scope_id,
        &scope.connection_id,
        &scope.claim_operation_id,
    ] {
        if !text(value) {
            return Err(Error::Invalid("command scope identity"));
        }
    }
    if scope.membership_generation == 0
        || scope.delivery_generation == 0
        || !scope
            .session_type
            .strip_prefix(&format!("{}/", scope.app))
            .is_some_and(|session| !session.is_empty() && !session.contains('/'))
    {
        return Err(Error::Invalid("command session binding"));
    }
    let manifest = deployment.domains.as_ref().ok_or(Error::Unknown)?;
    if manifest.apps.get(&scope.app).map(String::as_str) != Some(scope.domain.as_str())
        || !manifest.scopes.contains_key(&scope.domain)
    {
        return Err(Error::Invalid("command app domain binding"));
    }
    let value = json!({"kind":"command","proxyId":scope.proxy_id,"player":scope.player_uuid,"username":scope.username,"session":scope.session_id,"app":scope.app,"sessionType":scope.session_type,"domain":scope.domain,"scopeId":scope.scope_id,"connectionId":scope.connection_id,"claimOperationId":scope.claim_operation_id,"membershipGeneration":scope.membership_generation.to_string(),"deliveryGeneration":scope.delivery_generation.to_string()});
    validate_wire_value(&value).map_err(Error::Invalid)?;
    Ok(value.into())
}
