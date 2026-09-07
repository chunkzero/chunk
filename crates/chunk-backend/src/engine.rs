use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
};

use chunk_contract::{Deployment, Function, FunctionKind, Schema};
use chunk_js::{Cancellation, Execution, Invocation, Limits, Mode};
use chunk_store::{Commit, DocumentKey, KeyRange, Operation, Outcome, Revision, Snapshot, Storage, Write};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, watch};

use crate::{
    Error, Result,
    reads::{Dependency, Host, Trace},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Call {
    pub deployment: String,
    pub function: String,
    pub arguments: Value,
    pub caller: Value,
    /// Required for mutations. Retrying uses the same identity and request.
    pub operation: String,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub revision: Revision,
    /// Every result was evaluated against this revision's single snapshot.
    pub results: Vec<Value>,
}

const DEPLOYMENTS: &str = "__chunk_deployments";

#[derive(Serialize, Deserialize)]
struct StoredContract {
    digest: [u8; 32],
    tables: BTreeMap<String, Schema>,
}

struct State {
    store: Box<dyn Storage>,
    deployments: BTreeMap<String, Arc<Deployment>>,
    contracts: BTreeMap<String, StoredContract>,
}

#[derive(Clone)]
pub struct Backend {
    environment: Arc<str>,
    state: Arc<Mutex<State>>,
    workers: Arc<Semaphore>,
    subscriptions: Arc<Semaphore>,
    revision: watch::Sender<Revision>,
    #[cfg(test)]
    pub(crate) attempt_barrier: Option<Arc<tokio::sync::Barrier>>,
}

struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Backend {
    /// Storage must already hold the environment's exclusive writer capability.
    /// # Errors
    /// Returns storage errors and rejects an empty environment identity.
    pub fn new(environment: String, mut store: Box<dyn Storage>) -> Result<Self> {
        if environment.is_empty() || environment.len() > 256 {
            return Err(Error::Invalid("environment"));
        }
        let snapshot = store.snapshot()?;
        let revision = snapshot.revision;
        let contracts = snapshot
            .scan(&KeyRange {
                table: DEPLOYMENTS.into(),
                start: None,
                end: None,
            })?
            .into_iter()
            .map(|(id, document)| Ok((id.to_owned(), serde_json::from_value(document.value.clone())?)))
            .collect::<Result<BTreeMap<_, StoredContract>>>()?;
        if contracts.len() > 16 {
            return Err(Error::Invalid("retained deployment limit"));
        }
        Ok(Self {
            environment: environment.into(),
            state: Arc::new(Mutex::new(State {
                store,
                deployments: BTreeMap::new(),
                contracts,
            })),
            workers: Arc::new(Semaphore::new(4)),
            subscriptions: Arc::new(Semaphore::new(8)),
            revision: watch::channel(revision).0,
            #[cfg(test)]
            attempt_barrier: None,
        })
    }

    #[must_use]
    pub fn environment(&self) -> &str {
        &self.environment
    }

    fn state(&self) -> Result<MutexGuard<'_, State>> {
        self.state.lock().map_err(|_| Error::Poisoned)
    }

    /// Loads a pinned immutable version, checking its declared tables against stored data.
    /// Versions stay retained for this backend process; v1 caps the set at sixteen.
    /// # Errors
    /// Rejects changed content for an existing ID and incompatible existing documents.
    pub fn register(&self, deployment: Deployment) -> Result<()> {
        if deployment.id.is_empty()
            || deployment.id.len() > 128
            || deployment.source.len() > 4 * 1024 * 1024
            || deployment.functions.len() > 256
            || deployment.tables.len() > 128
            || serde_json::to_vec(&deployment)?.len() > 5 * 1024 * 1024
        {
            return Err(Error::Invalid("deployment limits"));
        }
        let mut state = self.state()?;
        if let Some(existing) = state.deployments.get(&deployment.id) {
            return if **existing == deployment {
                Ok(())
            } else {
                Err(Error::Invalid("immutable deployment changed"))
            };
        }
        let digest: [u8; 32] = Sha256::digest(serde_json::to_vec(&deployment)?).into();
        if let Some(contract) = state.contracts.get(&deployment.id) {
            if contract.digest != digest {
                return Err(Error::Invalid("immutable deployment changed"));
            }
            state.deployments.insert(deployment.id.clone(), Arc::new(deployment));
            return Ok(());
        }
        if state.contracts.len() >= 16 {
            return Err(Error::Busy);
        }
        let snapshot = state.store.snapshot()?;
        for (table, schema) in &deployment.tables {
            if table.starts_with("__chunk") {
                return Err(Error::Invalid("reserved table"));
            }
            let range = KeyRange {
                table: table.clone(),
                start: None,
                end: None,
            };
            for (_, document) in snapshot.scan(&range)? {
                if !schema.accepts(&document.value) {
                    return Err(Error::Contract);
                }
            }
        }
        let contract = StoredContract {
            digest,
            tables: deployment.tables.clone(),
        };
        let outcome = state.store.commit(Commit {
            expected: snapshot.revision,
            operation: Operation {
                id: format!("__chunk_deployment:{}", deployment.id),
                fingerprint: digest,
            },
            writes: vec![Write {
                key: DocumentKey::new(DEPLOYMENTS, &deployment.id)?,
                value: Some(serde_json::to_value(&contract)?),
            }],
            result: Value::Null,
        })?;
        state.contracts.insert(deployment.id.clone(), contract);
        state.deployments.insert(deployment.id.clone(), Arc::new(deployment));
        self.revision.send_replace(outcome.revision);
        Ok(())
    }

    fn resolve(&self, call: &Call) -> Result<(Arc<Deployment>, Function)> {
        if call.function.len() > 256
            || call.operation.len() > 256
            || call.operation.starts_with("__chunk")
            || serde_json::to_vec(&call)?.len() > 1024 * 1024
        {
            return Err(Error::Invalid("call limits"));
        }
        let state = self.state()?;
        let deployment = state.deployments.get(&call.deployment).ok_or(Error::Unknown)?.clone();
        let function = deployment.functions.get(&call.function).ok_or(Error::Unknown)?.clone();
        if !function.arguments.accepts(&call.arguments) {
            return Err(Error::Contract);
        }
        if function.kind == FunctionKind::Mutation && call.operation.is_empty() {
            return Err(Error::Invalid("operation ID required"));
        }
        Ok((deployment, function))
    }

    async fn evaluate(
        &self,
        call: &Call,
        deployment: Arc<Deployment>,
        function: &Function,
        snapshot: Arc<Snapshot>,
        cancellation: &Cancellation,
    ) -> Result<(Execution, Vec<Dependency>)> {
        let permit = self.workers.clone().try_acquire_owned().map_err(|_| Error::Busy)?;
        let trace = Trace::default();
        let host = Host {
            snapshot,
            deployment: deployment.clone(),
            trace: trace.clone(),
        };
        let invocation = Invocation {
            deployment: call.deployment.clone(),
            source: deployment.source.clone(),
            export: function.export.clone(),
            arguments: call.arguments.clone(),
            caller: call.caller.clone(),
            mode: match function.kind {
                FunctionKind::Query => Mode::Query,
                FunctionKind::Mutation => Mode::Mutation,
            },
        };
        let cancellation = cancellation.clone();
        let execution = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            chunk_js::execute(invocation, Box::new(host), Limits::default(), &cancellation)
        })
        .await??;
        if !function.result.accepts(&execution.value) {
            return Err(Error::Contract);
        }
        let reads = trace.lock().map_err(|_| Error::Poisoned)?.clone();
        Ok((execution, reads))
    }

    /// Executes a query or commits a mutation. A dropped future cancels speculative work.
    /// A cancelled/lost response can still follow a commit: recover with the same operation ID.
    /// # Errors
    /// Returns validation, capacity, execution, retry-limit or persistence failures.
    pub async fn call(&self, call: Call) -> Result<Outcome> {
        let cancellation = CancelOnDrop(Cancellation::default());
        let (deployment, function) = self.resolve(&call)?;
        let operation = Operation {
            id: call.operation.clone(),
            fingerprint: Sha256::digest(serde_json::to_vec(&(self.environment(), &call))?).into(),
        };
        for attempt in 0..8 {
            let _ = attempt;
            let snapshot = {
                let mut state = self.state()?;
                if function.kind == FunctionKind::Mutation
                    && let Some(outcome) = state.store.outcome(&operation)?
                {
                    return Ok(outcome);
                }
                Arc::new(state.store.snapshot()?)
            };
            let (execution, reads) = self
                .evaluate(&call, deployment.clone(), &function, snapshot.clone(), &cancellation.0)
                .await?;
            if function.kind == FunctionKind::Query {
                return Ok(Outcome {
                    revision: snapshot.revision,
                    result: execution.value,
                });
            }
            #[cfg(test)]
            if attempt == 0
                && let Some(barrier) = &self.attempt_barrier
            {
                barrier.wait().await;
            }
            let writes: Vec<_> = execution
                .writes
                .into_iter()
                .map(|write| {
                    Ok(Write {
                        key: DocumentKey::new(write.key.table, write.key.id)?,
                        value: write.value,
                    })
                })
                .collect::<Result<_>>()?;
            let mut state = self.state()?;
            if let Some(outcome) = state.store.outcome(&operation)? {
                return Ok(outcome);
            }
            let current = state.store.snapshot()?;
            if !reads.iter().all(|read| read.unchanged(&snapshot, &current)) {
                continue;
            }
            for write in &writes {
                if !deployment.tables.contains_key(&write.key.table) {
                    return Err(Error::Contract);
                }
                if let Some(value) = &write.value {
                    for version in state.contracts.values() {
                        if let Some(schema) = version.tables.get(&write.key.table)
                            && !schema.accepts(value)
                        {
                            return Err(Error::Contract);
                        }
                    }
                }
            }
            if cancellation.0.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let outcome = state.store.commit(Commit {
                expected: current.revision,
                operation: operation.clone(),
                writes,
                result: execution.value,
            })?;
            self.revision.send_replace(outcome.revision);
            return Ok(outcome);
        }
        Err(Error::Busy)
    }

    /// A subscription owns a bounded group of queries, possibly from different versions.
    /// Call `next` for the initial snapshot and after each update; reconnect creates a new group.
    /// # Errors
    /// Rejects mutations, oversized groups and excess subscriptions.
    pub fn subscribe(&self, calls: Vec<Call>) -> Result<QueryGroup> {
        if calls.is_empty() || calls.len() > 16 || serde_json::to_vec(&calls)?.len() > 1024 * 1024 {
            return Err(Error::Invalid("query group limits"));
        }
        for call in &calls {
            if self.resolve(call)?.1.kind != FunctionKind::Query {
                return Err(Error::Invalid("only queries can subscribe"));
            }
        }
        Ok(QueryGroup {
            backend: self.clone(),
            calls,
            revision: self.revision.subscribe(),
            previous: None,
            _permit: self
                .subscriptions
                .clone()
                .try_acquire_owned()
                .map_err(|_| Error::Busy)?,
        })
    }
}

pub struct QueryGroup {
    backend: Backend,
    calls: Vec<Call>,
    revision: watch::Receiver<Revision>,
    previous: Option<(Arc<Snapshot>, Vec<Dependency>)>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl QueryGroup {
    /// Coalesces commits while the consumer is slow; never mixes result revisions.
    /// # Errors
    /// Returns query errors or shutdown; the consumer must mark its old values stale.
    pub async fn next(&mut self) -> Result<Update> {
        loop {
            if self.previous.is_some() {
                self.revision.changed().await.map_err(|_| Error::Cancelled)?;
            }
            // Mark the observed revision before capturing the snapshot, so a concurrent commit
            // during evaluation remains pending on the next call.
            self.revision.borrow_and_update();
            let snapshot = Arc::new(self.backend.state()?.store.snapshot()?);
            if let Some((before, reads)) = &self.previous
                && reads.iter().all(|read| read.unchanged(before, &snapshot))
            {
                continue;
            }
            let cancellation = CancelOnDrop(Cancellation::default());
            let mut results = Vec::new();
            let mut dependencies = Vec::new();
            let mut bytes = 0;
            for call in &self.calls {
                let (deployment, function) = self.backend.resolve(call)?;
                let (execution, reads) = self
                    .backend
                    .evaluate(call, deployment, &function, snapshot.clone(), &cancellation.0)
                    .await?;
                bytes += serde_json::to_vec(&execution.value)?.len();
                if bytes > 1024 * 1024 {
                    return Err(Error::Invalid("query group result limit"));
                }
                results.push(execution.value);
                dependencies.extend(reads);
            }
            self.previous = Some((snapshot.clone(), dependencies));
            return Ok(Update {
                revision: snapshot.revision,
                results,
            });
        }
    }
}
