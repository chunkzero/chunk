use std::time::Duration;

use chunk_contract::{Deployment, Field, Function, FunctionKind, HookEvent, Schema, Visibility};
use serde_json::Value;

use crate::{Call, Error, Result};

pub(crate) const HOOK_TIMEOUT: Duration = Duration::from_secs(5);

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
