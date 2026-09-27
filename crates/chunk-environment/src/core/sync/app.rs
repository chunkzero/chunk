//! App function calls: queries read through the backend, mutations commit once per operation ID, and actions and a
//! gateway's hooks run once per operation ID `chunk:prepare` issued.

use super::{
    auth::{Class, Principal},
    errors,
    streams::Nudges,
};
use chunk_backend::{ActionId, Backend, Call, Update};
use chunk_contract::FunctionKind;
use chunk_proto::sync::v1::{Error, error::Code};
use chunk_store::Revision;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

/// Begins every operation ID `chunk:prepare` issues, which only actions and hooks accept.
pub(super) const PREPARED: &str = "prep:";

/// Rejects an operation ID `chunk:prepare` issued, for a write that isn't an action or hook.
pub(super) fn reject_prepared(operation: &str) -> Result<(), Error> {
    if operation.starts_with(PREPARED) {
        return Err(errors::invalid("operation IDs from chunk:prepare are only for actions and hooks"));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Kind {
    Function(FunctionKind),
    Hook,
}

pub(super) struct App {
    backend: Backend,
    /// Each deployment's public functions and hooks. A deployment ID names one immutable version.
    functions: Mutex<HashMap<String, Arc<BTreeMap<String, Kind>>>>,
    nudges: Nudges,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        Self { backend, functions: Mutex::default(), nudges: Nudges::default() }
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    pub fn nudges(&self) -> &Nudges {
        &self.nudges
    }

    /// Runs a query, a mutation under `operation`, which a retry repeats to get the committed outcome back, or an
    /// action or hook under the prepared `operation`, which a retry repeats to get its outcome back. A committed
    /// mutation nudges the streams `principal` opened.
    pub async fn call(&self, principal: &Principal, operation: String, call: Call) -> Result<Update, Error> {
        let result = match self.kind(&call).await? {
            Kind::Function(FunctionKind::Query) => self.backend.query(call).await,
            Kind::Function(FunctionKind::Mutation) if operation.is_empty() => {
                return Err(errors::invalid("a mutation requires an operation ID"));
            }
            Kind::Function(FunctionKind::Mutation) => {
                reject_prepared(&operation)?;
                let result = self.backend.mutate(operation, call).await;
                if let Ok(update) = &result {
                    self.nudges.nudge(&principal.credential, update.revision);
                }
                result
            }
            Kind::Function(FunctionKind::Action) => return self.effect(false, &operation, call).await,
            Kind::Hook if matches!(principal.class, Class::Gateway { .. }) => {
                return self.effect(true, &operation, call).await;
            }
            Kind::Hook => return Err(errors::denied("only a gateway runs hooks")),
        };
        result.map_err(|failure| errors::backend(&failure))
    }

    /// Runs an action, or a hook, under `operation` and returns its outcome, which touches no single commit. The run
    /// outlives a dropped call, so a retry finds its outcome.
    async fn effect(&self, hook: bool, operation: &str, call: Call) -> Result<Update, Error> {
        let Some(id) = operation.strip_prefix(PREPARED) else {
            return Err(errors::invalid("an effectful call requires an operation ID from chunk:prepare"));
        };
        let unknown = || errors::error(Code::OutcomeUnknown, "core didn't prepare this operation ID");
        let id: ActionId = id.parse().map_err(|_| unknown())?;
        let backend = self.backend.clone();
        let run = tokio::spawn(async move {
            let mut handle =
                if hook { backend.start_hook(id, call).await } else { backend.start_action(id, call).await }?;
            handle.outcome().await
        });
        let outcome = run.await.map_err(|_| errors::error(Code::OutcomeUnknown, "the effectful call's task failed"))?;
        let json = outcome.map_err(|failure| errors::backend(&failure))?;
        Ok(Update { revision: Revision(0), json })
    }

    async fn kind(&self, call: &Call) -> Result<Kind, Error> {
        let functions = if let Some(functions) = self.cached(call.deployment.as_str()) {
            functions
        } else {
            let failed = |failure: chunk_backend::Error| errors::backend(&failure);
            let deployment = call.deployment.clone();
            let functions = self.backend.functions(deployment.clone()).await.map_err(failed)?;
            let manifest = self.backend.domain_manifest(deployment).await.map_err(failed)?;
            let hooks = manifest.into_iter().flat_map(|manifest| manifest.hooks.into_keys());
            let mut kinds: BTreeMap<_, _> = hooks.map(|hook| (hook, Kind::Hook)).collect();
            kinds.extend(functions.into_iter().map(|(name, kind)| (name, Kind::Function(kind))));
            let kinds = Arc::new(kinds);
            if let Ok(mut cache) = self.functions.lock() {
                cache.insert(call.deployment.as_str().to_owned(), kinds.clone());
            }
            kinds
        };
        let kind = functions.get(&call.function).copied();
        kind.ok_or_else(|| errors::error(Code::Contract, "the deployment has no such public function or hook"))
    }

    fn cached(&self, deployment: &str) -> Option<Arc<BTreeMap<String, Kind>>> {
        self.functions.lock().ok()?.get(deployment).cloned()
    }
}
