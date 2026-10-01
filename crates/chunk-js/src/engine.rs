use std::collections::BTreeMap;

use crate::{
    Cancellation, Error, Execution, Invocation, Json, Limits, ReadHost,
    capabilities::Capabilities,
    deadline::Deadline,
    isolate::Runtime,
    model::bounds,
    runtime::{Prepared, Runner},
};

/// An immutable deployment identity within one environment.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeploymentId(String);

impl DeploymentId {
    /// # Errors
    /// Rejects an empty identity or more than 128 bytes.
    pub fn new(id: impl Into<String>) -> Result<Self, Error> {
        let id = id.into();
        if id.is_empty() || id.len() > bounds::NAME_BYTES {
            return Err(Error::Invalid("invalid deployment"));
        }
        Ok(Self(id))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

struct Resident {
    source: String,
    limits: Limits,
    env: Json,
    runtime: Option<Runtime>,
}

/// Owns all resident deployment runtimes on the environment's engine thread.
/// Construct, execute, release and drop on that same thread. Calls are serialized
/// through exclusive mutable access; the backend supplies admission and queues.
///
/// ```compile_fail
/// let engine = chunk_js::Engine::new().unwrap();
/// std::thread::spawn(move || drop(engine));
/// ```
pub struct Engine {
    deployments: BTreeMap<DeploymentId, Resident>,
    executor: tokio::runtime::Runtime,
    deadline: Deadline,
}

impl Engine {
    /// Initialize V8 on the common parent thread before spawning engine threads.
    /// Further calls are harmless; a single-threaded embedder can just call `new`.
    pub fn init_platform() {
        deno_core::JsRuntime::init_platform(None);
    }

    /// Creates one executor and one watchdog, shared by every deployment.
    /// # Errors
    /// Reports executor or watchdog creation failures.
    pub fn new() -> Result<Self, Error> {
        Self::init_platform();
        Ok(Self {
            deployments: BTreeMap::new(),
            executor: tokio::runtime::Builder::new_current_thread().enable_all().build()?,
            deadline: Deadline::new()?,
        })
    }

    /// Loads an immutable bundled module without invocation capabilities, whose `ctx.env` is empty.
    /// # Errors
    /// Rejects duplicate identities, source/limit violations and failed initialization.
    pub fn register(&mut self, id: DeploymentId, source: String, limits: Limits) -> Result<(), Error> {
        self.register_with_env(id, source, limits, Json::empty())
    }

    /// Loads an immutable bundled module whose queries and mutations read `env`, a JSON object of strings, as
    /// `ctx.env`. Actions read the env of their invocation instead.
    /// # Errors
    /// As [`Self::register`].
    pub fn register_with_env(
        &mut self,
        id: DeploymentId,
        source: String,
        limits: Limits,
        env: Json,
    ) -> Result<(), Error> {
        if self.deployments.contains_key(&id) {
            return Err(Error::Invalid("deployment already registered"));
        }
        if source.len() > bounds::SOURCE_BYTES {
            return Err(Error::Invalid("invalid source"));
        }
        if !(bounds::MIN_HEAP_BYTES..=bounds::MAX_HEAP_BYTES).contains(&limits.heap_bytes)
            || limits.execution.is_zero()
            || limits.execution > bounds::MAX_EXECUTION
        {
            return Err(Error::Invalid("limits outside local execution budget"));
        }
        let runtime = Runtime::load(
            &self.executor,
            &self.deadline,
            &format!("chunk:deployment/{}", id.as_str()),
            &source,
            limits,
            &Cancellation::default(),
        )?;
        self.deployments.insert(id, Resident { source, limits, env, runtime: Some(runtime) });
        Ok(())
    }

    /// Runs against fresh host capabilities, retaining globals across ordinary application errors.
    /// Recycles a terminated runtime or one that has completed 10,000 calls; other
    /// deployments remain resident and retain their own state.
    /// # Errors
    /// Reports unknown deployments, invalid input, execution errors and budgets.
    pub fn execute(
        &mut self,
        id: &DeploymentId,
        invocation: Invocation,
        host: Box<dyn ReadHost>,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        let resident = self.deployments.get_mut(id).ok_or(Error::UnknownDeployment)?;
        if invocation.export.is_empty() || invocation.export.len() > bounds::NAME_BYTES {
            return Err(Error::Invalid("invalid export"));
        }
        let caller = invocation.caller;
        let arguments = invocation.arguments;
        if caller.as_str().len() > bounds::JSON_BYTES || arguments.as_str().len() > bounds::JSON_BYTES {
            return Err(Error::Invalid("input exceeds size limit"));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if resident.runtime.as_ref().is_some_and(|runtime| runtime.calls() >= bounds::RUNTIME_CALLS) {
            resident.runtime = None;
        }
        let mut runtime = match resident.runtime.take() {
            Some(runtime) => runtime,
            None => Runtime::load(
                &self.executor,
                &self.deadline,
                &format!("chunk:deployment/{}", id.as_str()),
                &resident.source,
                resident.limits,
                cancellation,
            )?,
        };
        let prepared = Prepared {
            export: invocation.export,
            caller,
            arguments,
            timestamp: invocation.timestamp,
            seed: invocation.seed,
            env: resident.env.clone(),
            runner: Runner::Transaction(Capabilities {
                generation: runtime.calls() + 1,
                host,
                mode: invocation.mode,
                cancellation: cancellation.clone(),
                writes: BTreeMap::new(),
                jobs: Vec::new(),
                calls: 0,
                write_bytes: 0,
            }),
        };
        let result = runtime.execute(&self.executor, &self.deadline, prepared, resident.limits, cancellation);
        if !matches!(result, Err(Error::Cancelled | Error::Deadline | Error::Heap)) {
            resident.runtime = Some(runtime);
        }
        result
    }

    /// Runs an action in a fresh isolate. Call this on a bounded worker thread,
    /// never the foreground transaction actor. No snapshot capability is installed.
    /// # Errors
    /// Reports invalid input, cancellation, resource limits and application failures.
    pub fn execute_action(
        &mut self,
        id: &DeploymentId,
        invocation: crate::ActionInvocation,
        host: std::rc::Rc<dyn crate::ActionHost>,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        let resident = self.deployments.get_mut(id).ok_or(Error::UnknownDeployment)?;
        if invocation.export.is_empty()
            || invocation.export.len() > bounds::NAME_BYTES
            || invocation.id.len() > bounds::INVOCATION_ID_BYTES
            || invocation.arguments.as_str().len() > bounds::JSON_BYTES
            || invocation.caller.as_str().len() > bounds::JSON_BYTES
        {
            return Err(Error::Invalid("action input limit"));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if resident.runtime.as_ref().is_some_and(|runtime| runtime.calls() != 0) {
            return Err(Error::Invalid("action requires a fresh worker"));
        }
        let deadline = invocation.deadline;
        let execution = deadline.saturating_duration_since(std::time::Instant::now());
        if execution.is_zero() {
            return Err(Error::Deadline);
        }
        if execution > bounds::MAX_EXECUTION {
            return Err(Error::Invalid("action duration limit"));
        }
        let limits = Limits { execution, ..resident.limits };
        let mut runtime = resident.runtime.take().ok_or(Error::Invalid("action worker already used"))?;
        let prepared = Prepared {
            export: invocation.export,
            caller: invocation.caller,
            arguments: invocation.arguments,
            timestamp: invocation.timestamp,
            seed: invocation.seed,
            env: invocation.env,
            runner: Runner::Action(crate::actions::ActionCapabilities::new(
                invocation.id,
                host,
                cancellation.clone(),
                limits.execution,
            )),
        };
        let result = runtime.execute(&self.executor, &self.deadline, prepared, limits, cancellation);
        if cancellation.is_cancelled() {
            Err(Error::Cancelled)
        } else if std::time::Instant::now() >= deadline {
            Err(Error::Deadline)
        } else {
            result
        }
    }

    /// Releases the runtime and retained source immediately, in any registration order.
    /// Returns whether the deployment was resident. The backend must drain its references first.
    pub fn release(&mut self, id: &DeploymentId) -> bool {
        self.deployments.remove(id).is_some()
    }
}
