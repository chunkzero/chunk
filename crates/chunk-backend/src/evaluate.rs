use std::{cell::RefCell, rc::Rc, sync::Arc};

use chunk_contract::{Deployment, Function, validate_wire_value};
use chunk_js::{Cancellation, Engine, Execution, Invocation, Mode};
use sha2::{Digest, Sha256};

use crate::{
    Error, Result,
    reads::{Dependencies, Host, View},
    service::Call,
};

/// A resolved query or mutation, ready to run on any engine that has its deployment.
pub(crate) struct Target<'a> {
    pub call: &'a Call,
    pub function: Option<&'a Function>,
    pub contract: Option<Arc<Deployment>>,
    /// Redacts what it logs, since an action may pass it a secret.
    pub secrets: &'a crate::effects::SecretSlot,
}

/// Runs one transaction against `view`, returning what it read even when it fails.
/// `context` fixes the time, seed and operation of a mutation across retries.
pub(crate) fn evaluate(
    js: &mut Engine,
    target: Target<'_>,
    mode: Mode,
    view: Arc<View>,
    cancellation: &Cancellation,
    context: Option<(i64, u64, String)>,
) -> (Result<Execution>, Dependencies) {
    let Target { call, function, contract, secrets } = target;
    let trace = Rc::new(RefCell::new(Dependencies::default()));
    let operation = context.as_ref().map(|(_, _, operation)| operation.clone());
    let (timestamp, seed) = context.map_or_else(
        || {
            let seed =
                u64::from_be_bytes(Sha256::digest(call.function.as_bytes())[..8].try_into().expect("digest prefix"));
            (view.base.timestamp, seed)
        },
        |(timestamp, seed, _)| (timestamp, seed),
    );
    let host = Host { operation, view, trace: trace.clone(), contract, budget: crate::reads::read_budget() };
    let execution = js
        .execute(
            &call.deployment,
            Invocation {
                export: function.map_or_else(|| call.function.clone(), |f| f.export.clone()),
                arguments: call.arguments.clone(),
                caller: call.caller.clone(),
                mode,
                timestamp,
                seed,
            },
            Box::new(host),
            cancellation,
        )
        .map_err(Error::from)
        .and_then(|mut execution| {
            for log in &execution.logs {
                console!(
                    log.level.as_str(),
                    deployment = call.deployment.as_str(),
                    function = call.function,
                    message = secrets.redact(log.message.clone())
                );
            }
            let mut value = serde_json::from_str(&execution.value)?;
            if let Some(function) = function {
                function.result.normalize_api(&mut value);
            }
            validate_wire_value(&value).map_err(Error::Invalid)?;
            if function.is_some_and(|f| !f.result.accepts(&value)) {
                return Err(Error::Contract);
            }
            execution.value = serde_json::to_string(&value)?;
            Ok(execution)
        });
    let dependencies = std::mem::take(&mut *trace.borrow_mut());
    (execution, dependencies)
}
