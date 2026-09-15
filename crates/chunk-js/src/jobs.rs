use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde::Deserialize;

use crate::{Mode, capabilities::Capabilities};

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ScheduleIntent {
    RunAt { id: Option<String>, at: i64, function: String, arguments: serde_json::Value },
    Cancel { id: String },
    Retry { id: String, at: i64, acknowledge_possible_effects: bool },
}

#[op2]
#[string]
fn op_chunk_schedule(state: &mut OpState, generation: u32, #[string] request: &str) -> Result<String, JsErrorBox> {
    let capabilities = state
        .borrow_mut::<Option<Capabilities>>()
        .as_mut()
        .filter(|capabilities| {
            capabilities.generation == generation
                && capabilities.mode == Mode::Mutation
                && !capabilities.cancellation.is_cancelled()
        })
        .ok_or_else(|| JsErrorBox::generic("Mutation scheduler capability expired or unavailable"))?;
    if request.len() > 64 * 1024 || capabilities.jobs.len() >= 16 {
        return Err(JsErrorBox::range_error("Scheduled intent limit"));
    }
    let mut intent: ScheduleIntent = serde_json::from_str(request).map_err(JsErrorBox::from_err)?;
    let value = if let ScheduleIntent::RunAt { id, .. } = &mut intent {
        if id.is_some() {
            return Err(JsErrorBox::generic("Job identity is host-owned"));
        }
        let generated = capabilities
            .host
            .schedule_id(u32::try_from(capabilities.jobs.len()).expect("bounded intents"))
            .map_err(JsErrorBox::generic)?;
        *id = Some(generated.clone());
        serde_json::Value::String(generated)
    } else {
        serde_json::Value::Null
    };
    capabilities.jobs.push(intent);
    serde_json::to_string(&value).map_err(JsErrorBox::from_err)
}

deno_core::extension!(chunk_scheduler, ops = [op_chunk_schedule]);
