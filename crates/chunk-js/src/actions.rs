use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};

use deno_core::{OpState, op2};
use deno_error::JsErrorBox;
use serde::Deserialize;

use crate::{Cancellation, Json, Mode, model::bounds};

/// Trusted per-invocation bridge. The host owns caller/deployment binding and
/// allocates a distinct transaction for every call, including concurrent calls.
pub trait ActionHost: 'static {
    fn call(
        &self,
        sequence: u32,
        mode: Mode,
        function: String,
        arguments: Json,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>>>>;

    fn fetch(
        &self,
        _sequence: u32,
        _request: crate::HttpRequest,
    ) -> Pin<Box<dyn Future<Output = Result<crate::HttpOutcome, String>>>> {
        Box::pin(async { Err("HTTP effects unavailable".into()) })
    }

    fn platform(&self, _sequence: u32, _request: Json) -> Pin<Box<dyn Future<Output = Result<String, String>>>> {
        Box::pin(async { Err("Platform effects unavailable".into()) })
    }
}

pub struct ActionInvocation {
    pub id: String,
    pub export: String,
    pub arguments: Json,
    pub caller: Json,
    pub timestamp: i64,
    pub seed: u64,
    pub deadline: Instant,
    /// The JSON object `ctx.env` reads, with string values.
    pub env: Json,
}

pub(crate) struct ActionCapabilities {
    pub id: String,
    pub host: Rc<dyn ActionHost>,
    pub cancellation: Cancellation,
    pub deadline: Instant,
    calls: u32,
    pending: usize,
}

impl ActionCapabilities {
    pub fn new(id: String, host: Rc<dyn ActionHost>, cancellation: Cancellation, duration: Duration) -> Self {
        Self { id, host, cancellation, deadline: Instant::now() + duration, calls: 0, pending: 0 }
    }
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Effect {
    Sleep { milliseconds: u64 },
    Fetch { request: crate::HttpRequest },
    Platform { request: serde_json::Value },
    Query { function: String, arguments: serde_json::Value },
    Mutation { function: String, arguments: serde_json::Value },
}

struct Pending(Rc<RefCell<OpState>>);
impl Drop for Pending {
    fn drop(&mut self) {
        if let Some(capabilities) = self.0.borrow_mut().borrow_mut::<Option<ActionCapabilities>>() {
            capabilities.pending -= 1;
        }
    }
}

#[op2]
#[string]
fn op_chunk_action_id(state: &mut OpState) -> Option<String> {
    state.borrow::<Option<ActionCapabilities>>().as_ref().map(|capabilities| capabilities.id.clone())
}

#[op2]
#[string]
async fn op_chunk_action(state: Rc<RefCell<OpState>>, #[string] request: String) -> Result<String, JsErrorBox> {
    if request.len() > bounds::JSON_BYTES {
        return Err(JsErrorBox::range_error("Action effect input limit"));
    }
    let effect: Effect = serde_json::from_str(&request).map_err(JsErrorBox::from_err)?;
    let (host, sequence, cancellation, deadline) = {
        let mut state = state.borrow_mut();
        let capabilities = state
            .borrow_mut::<Option<ActionCapabilities>>()
            .as_mut()
            .ok_or_else(|| JsErrorBox::generic("Action capability expired"))?;
        if capabilities.cancellation.is_cancelled() || Instant::now() >= capabilities.deadline {
            return Err(JsErrorBox::generic("Action capability expired"));
        }
        if capabilities.calls >= 256 || capabilities.pending >= 8 {
            return Err(JsErrorBox::range_error("Action effect capacity reached"));
        }
        capabilities.calls += 1;
        capabilities.pending += 1;
        (capabilities.host.clone(), capabilities.calls, capabilities.cancellation.clone(), capabilities.deadline)
    };
    let _pending = Pending(state);
    let effect = async {
        match effect {
            Effect::Fetch { request } => {
                serde_json::to_string(&host.fetch(sequence, request).await?).map_err(|_| "Invalid HTTP outcome".into())
            }
            Effect::Platform { request } => host.platform(sequence, request.into()).await,
            Effect::Sleep { milliseconds } => {
                if milliseconds > 30_000 {
                    return Err("Action sleep exceeds duration limit".into());
                }
                tokio::time::sleep(Duration::from_millis(milliseconds)).await;
                Ok("null".into())
            }
            Effect::Query { function, arguments } => host.call(sequence, Mode::Query, function, arguments.into()).await,
            Effect::Mutation { function, arguments } => {
                host.call(sequence, Mode::Mutation, function, arguments.into()).await
            }
        }
    };
    tokio::select! {
        biased;
        () = cancellation.expired(deadline) => Err(JsErrorBox::generic("Action capability expired; accepted effects may have completed")),
        result = effect => {
            let result = result.map_err(JsErrorBox::generic)?;
            if result.len() > bounds::JSON_BYTES { return Err(JsErrorBox::range_error("Action effect result limit")); }
            Ok(result)
        }
    }
}

/// The JSON object `ctx.env` reads during the current invocation.
pub(crate) struct Env(pub Json);

#[op2]
#[string]
fn op_chunk_env(state: &mut OpState) -> String {
    state.borrow::<Env>().0.as_str().to_owned()
}

deno_core::extension!(chunk_actions, ops = [op_chunk_action, op_chunk_action_id, op_chunk_env]);

#[cfg(test)]
mod tests;
