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
}

pub struct ActionInvocation {
    pub id: String,
    pub export: String,
    pub arguments: Json,
    pub caller: Json,
    pub timestamp: i64,
    pub seed: u64,
    pub deadline: Instant,
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
    let expired = async {
        loop {
            if cancellation.is_cancelled() || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    };
    tokio::select! {
        biased;
        () = expired => Err(JsErrorBox::generic("Action capability expired; accepted effects may have completed")),
        result = effect => {
            let result = result.map_err(JsErrorBox::generic)?;
            if result.len() > bounds::JSON_BYTES { return Err(JsErrorBox::range_error("Action effect result limit")); }
            Ok(result)
        }
    }
}

deno_core::extension!(chunk_actions, ops = [op_chunk_action, op_chunk_action_id]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DeploymentId, Engine, Error, Limits};

    struct Host;
    impl ActionHost for Host {
        fn call(&self, _: u32, _: Mode, _: String, _: Json) -> Pin<Box<dyn Future<Output = Result<String, String>>>> {
            Box::pin(async { Err("unexpected transaction".into()) })
        }
    }

    #[test]
    fn action_deadline_expires_a_sleep_without_retaining_its_runtime() {
        let mut engine = Engine::new().unwrap();
        let id = DeploymentId::new("deadline").unwrap();
        engine
            .register(
                id.clone(),
                "export async function work(ctx) { await ctx.sleep(1000); return 1; }".into(),
                Limits::default(),
            )
            .unwrap();
        let result = engine.execute_action(
            &id,
            ActionInvocation {
                id: "one".into(),
                export: "work".into(),
                arguments: serde_json::Value::Null.into(),
                caller: serde_json::Value::Null.into(),
                timestamp: 0,
                seed: 0,
                deadline: Instant::now() + Duration::from_millis(50),
            },
            Rc::new(Host),
            &Cancellation::default(),
        );
        assert!(matches!(result, Err(Error::Deadline)), "{result:?}");
        assert!(engine.release(&id));
    }
}
