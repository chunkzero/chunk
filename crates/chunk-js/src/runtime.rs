use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};

use crate::{
    Cancellation, Error, Execution, Json, Limits, Write,
    capabilities::{Capabilities, chunk_capabilities},
    deadline::Deadline,
    model::bounds,
    termination::{Reason, Termination},
};

mod snapshot_sources {
    include!(concat!(env!("OUT_DIR"), "/snapshot_sources.rs"));
}

pub(crate) struct Prepared {
    pub export: String,
    pub caller: Json,
    pub arguments: Json,
    pub timestamp: i64,
    pub seed: u64,
    pub capabilities: Option<Capabilities>,
    pub action: Option<crate::actions::ActionCapabilities>,
}

pub(crate) struct State {
    // Persistent handles must drop before their isolate.
    run: Option<v8::Global<v8::Function>>,
    namespace: Option<v8::Global<v8::Object>>,
    pub runtime: JsRuntime,
    termination: Termination,
    pub calls: u32,
}

impl State {
    pub(crate) fn execute(
        &mut self,
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        prepared: Prepared,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        self.calls += 1;
        let Prepared { export, caller, arguments, capabilities, action, timestamp, seed } = prepared;
        let is_action = action.is_some();
        crate::profile::begin(&mut self.runtime, timestamp, seed)?;
        self.runtime.op_state().borrow_mut().put(capabilities);
        self.runtime.op_state().borrow_mut().put(action);
        let result = self.guarded(deadline, limits, cancellation, |engine| {
            let result = executor.block_on(engine.invoke(&export, caller.as_str(), arguments.as_str()));
            if !is_action {
                executor.block_on(engine.drain())?;
            }
            result
        });
        let logs = crate::profile::end(&mut self.runtime);
        let capabilities = self.runtime.op_state().borrow_mut().borrow_mut::<Option<Capabilities>>().take();
        self.runtime.op_state().borrow_mut().borrow_mut::<Option<crate::actions::ActionCapabilities>>().take();
        let value = result?;
        let jobs = capabilities.as_ref().map_or_else(Vec::new, |capabilities| capabilities.jobs.clone());
        let writes = capabilities
            .into_iter()
            .flat_map(|capabilities| capabilities.writes)
            .map(|(key, write)| Write { key, value: write.value })
            .collect();
        Ok(Execution { logs, value, writes, jobs })
    }

    pub(crate) fn new(limits: Limits) -> Self {
        let termination = Termination::default();
        let mut extensions = crate::extensions::web();
        extensions.extend([
            chunk_capabilities::init(),
            crate::profile::chunk_profile::init(),
            crate::actions::chunk_actions::init(),
            crate::jobs::chunk_scheduler::init(),
        ]);
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions,
            startup_snapshot: Some(include_bytes!(concat!(env!("OUT_DIR"), "/snapshot.bin"))),
            residual_lazy_js_sources: snapshot_sources::JS,
            residual_lazy_esm_sources: snapshot_sources::ESM,
            create_params: Some(
                v8::Isolate::create_params()
                    .heap_limits(0, limits.heap_bytes)
                    .array_buffer_allocator(crate::allocator::bounded(limits.heap_bytes, termination.clone())),
            ),
            ..Default::default()
        });
        runtime.op_state().borrow_mut().put(None::<Capabilities>);
        runtime.op_state().borrow_mut().put(None::<crate::actions::ActionCapabilities>);
        crate::profile::initialize(&mut runtime);
        let heap_signal = termination.clone();
        let handle = runtime.v8_isolate().thread_safe_handle();
        runtime.add_near_heap_limit_callback(move |limit, _| {
            heap_signal.record(Reason::Heap);
            handle.terminate_execution();
            limit + bounds::EMERGENCY_HEAP_BYTES
        });
        Self { run: None, namespace: None, runtime, termination, calls: 0 }
    }

    pub(crate) fn initialize_on(
        &mut self,
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        specifier: &str,
        source: &str,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<(), Error> {
        self.guarded(deadline, limits, cancellation, |state| executor.block_on(state.initialize(specifier, source)))
    }

    fn guarded<T>(
        &mut self,
        deadline: &Deadline,
        limits: Limits,
        cancellation: &Cancellation,
        run: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let handle = self.runtime.v8_isolate().thread_safe_handle();
        let guard = deadline.arm(handle, cancellation.clone(), limits.execution, self.termination.clone());
        let result = run(self);
        drop(guard);
        self.termination.take().map_or(result, Err)
    }

    async fn initialize(&mut self, specifier: &str, source: &str) -> Result<(), Error> {
        self.runtime.execute_script("chunk:web", include_str!("web.js")).map_err(js_error)?;
        let run = self.runtime.execute_script("chunk:bootstrap", include_str!("bootstrap.js")).map_err(js_error)?;
        {
            deno_core::scope!(scope, &mut self.runtime);
            let run = v8::Local::new(scope, run);
            self.run = Some(v8::Global::new(scope, v8::Local::<v8::Function>::try_from(run).map_err(js_error)?));
        }
        let module = self
            .runtime
            .load_main_es_module_from_code(&ModuleSpecifier::parse(specifier).map_err(js_error)?, source.to_owned())
            .await
            .map_err(js_error)?;
        let evaluation = self.runtime.mod_evaluate(module);
        self.runtime.with_event_loop_promise(evaluation, PollEventLoopOptions::default()).await.map_err(js_error)?;
        self.drain().await?;
        self.namespace = Some(self.runtime.get_module_namespace(module).map_err(js_error)?);
        Ok(())
    }

    async fn drain(&mut self) -> Result<(), Error> {
        self.runtime.run_event_loop(PollEventLoopOptions::default()).await.map_err(js_error)
    }

    async fn invoke(&mut self, export: &str, caller: &str, arguments: &str) -> Result<String, Error> {
        let args = {
            deno_core::scope!(scope, &mut self.runtime);
            let namespace = v8::Local::new(scope, self.namespace.as_ref().expect("initialized"));
            let key = v8::String::new(scope, export).ok_or(Error::Heap)?;
            let function = namespace.get(scope, key.into()).ok_or(Error::Invalid("missing export"))?;
            if !function.is_function() {
                return Err(Error::Invalid("missing export"));
            }
            let function = v8::Local::<v8::Function>::try_from(function).map_err(js_error)?;
            let caller = v8::String::new(scope, caller).ok_or(Error::Heap)?;
            let arguments = v8::String::new(scope, arguments).ok_or(Error::Heap)?;
            let generation = v8::Integer::new_from_unsigned(scope, self.calls);
            [
                v8::Global::new(scope, v8::Local::<v8::Value>::from(function)),
                v8::Global::new(scope, v8::Local::<v8::Value>::from(caller)),
                v8::Global::new(scope, v8::Local::<v8::Value>::from(generation)),
                v8::Global::new(scope, v8::Local::<v8::Value>::from(arguments)),
            ]
        };
        let call = self.runtime.call_with_args(self.run.as_ref().expect("initialized"), &args);
        let output =
            self.runtime.with_event_loop_promise(call, PollEventLoopOptions::default()).await.map_err(js_error)?;
        deno_core::scope!(scope, &mut self.runtime);
        let output = v8::Local::new(scope, output);
        let output = v8::Local::<v8::String>::try_from(output).map_err(js_error)?;
        let encoded = output.to_rust_string_lossy(scope);
        if encoded.len() > bounds::JSON_BYTES {
            return Err(Error::Invalid("result exceeds size limit"));
        }
        // Durable outcomes use serde_json too; reject unsupported depth and Unicode
        // before speculative writes can enter the commit pipeline.
        serde_json::from_str::<serde_json::Value>(&encoded).map_err(js_error)?;
        Ok(encoded)
    }
}

fn js_error(error: impl std::fmt::Display) -> Error {
    Error::JavaScript(error.to_string())
}
