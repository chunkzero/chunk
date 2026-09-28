use chunk_contract::{Deployment, Effect, Field, Schema, validate_wire_value};
use chunk_js::Json;

use crate::{CommandScope, Error, Result};

pub(crate) fn validate(deployment: &Deployment, scope: &CommandScope, request: &Json) -> Result<(Json, Schema, bool)> {
    if request.as_str().len() > 64 * 1024 {
        return Err(Error::Invalid("command effect size"));
    }
    let mut effect: Effect = serde_json::from_str(request.as_str())?;
    let receipt = !matches!(effect, Effect::SessionCall { .. });
    let mut result = Schema::Object {
        fields: [
            ("state".into(), Field { schema: Schema::Literal { value: "accepted".into() }, optional: false }),
            ("operationId".into(), Field { schema: Schema::String, optional: false }),
        ]
        .into(),
    };
    match &mut effect {
        Effect::Message { text: value } | Effect::ActionBar { text: value } => display_text(value)?,
        Effect::Title { title, subtitle } => {
            display_text(title)?;
            if let Some(subtitle) = subtitle {
                display_text(subtitle)?;
            }
        }
        Effect::Enter { destination } => {
            for value in [&destination.key, &destination.session_type, &destination.machine_profile] {
                if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
                    return Err(Error::Invalid("command destination"));
                }
            }
        }
        Effect::SessionCall { method, arguments } | Effect::SessionSend { method, arguments } => {
            if method.app != scope.app || format!("{}/{}", method.app, method.session) != scope.session_type {
                return Err(Error::Invalid("command session method scope"));
            }
            let declaration = deployment
                .contracts
                .session_methods
                .as_ref()
                .and_then(|methods| {
                    methods.methods.iter().find(|candidate| {
                        candidate.app == method.app
                            && candidate.session == method.session
                            && candidate.name == method.name
                    })
                })
                .ok_or(Error::Unknown)?;
            declaration.arguments.normalize_api(arguments);
            validate_wire_value(arguments).map_err(Error::Invalid)?;
            if !declaration.arguments.accepts(arguments) {
                return Err(Error::Contract);
            }
            if !receipt {
                result = declaration.result.clone();
            }
        }
    }
    Ok((serde_json::to_value(effect)?.into(), result, receipt))
}

fn display_text(value: &str) -> Result<()> {
    if value.len() > 12 * 1024 || value.encode_utf16().count() > 4096 || value.contains('\0') {
        return Err(Error::Invalid("command text limit"));
    }
    Ok(())
}
