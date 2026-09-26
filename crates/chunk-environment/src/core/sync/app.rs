//! App function calls: queries read through the backend, mutations commit once per operation ID.

use super::errors;
use chunk_backend::{Backend, Call, Update};
use chunk_contract::FunctionKind;
use chunk_proto::sync::v1::{Error, error::Code};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
};

pub(super) struct App {
    backend: Backend,
    /// Each deployment's public functions by kind. A deployment ID names one immutable version.
    functions: Mutex<HashMap<String, Arc<BTreeMap<String, FunctionKind>>>>,
}

impl App {
    pub fn new(backend: Backend) -> Self {
        Self { backend, functions: Mutex::default() }
    }

    pub fn backend(&self) -> &Backend {
        &self.backend
    }

    /// Runs a query, or a mutation under `operation`, which a retry repeats to get the committed outcome back.
    pub async fn call(&self, operation: String, call: Call) -> Result<Update, Error> {
        let result = match self.kind(&call).await? {
            FunctionKind::Query => self.backend.query(call).await,
            FunctionKind::Mutation if operation.is_empty() => {
                return Err(errors::invalid("a mutation requires an operation ID"));
            }
            FunctionKind::Mutation => self.backend.mutate(operation, call).await,
            FunctionKind::Action => return Err(errors::invalid("actions are not served over the sync protocol yet")),
        };
        result.map_err(|failure| errors::backend(&failure))
    }

    async fn kind(&self, call: &Call) -> Result<FunctionKind, Error> {
        let functions = if let Some(functions) = self.cached(call.deployment.as_str()) {
            functions
        } else {
            let functions = self.backend.functions(call.deployment.clone()).await;
            let functions = Arc::new(functions.map_err(|failure| errors::backend(&failure))?);
            if let Ok(mut cache) = self.functions.lock() {
                cache.insert(call.deployment.as_str().to_owned(), functions.clone());
            }
            functions
        };
        let kind = functions.get(&call.function).copied();
        kind.ok_or_else(|| errors::error(Code::Contract, "the deployment has no such public function"))
    }

    fn cached(&self, deployment: &str) -> Option<Arc<BTreeMap<String, FunctionKind>>> {
        self.functions.lock().ok()?.get(deployment).cloned()
    }
}
