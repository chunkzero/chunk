use std::collections::BTreeMap;

use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};

use crate::{
    Cancellation, Error, Execution, Invocation, Limits, ReadHost, Write,
    capabilities::{Capabilities, chunk_capabilities},
    deadline::Deadline,
    model::bounds,
    termination::{Reason, Termination},
};

pub(crate) struct Worker {
    source: String,
    specifier: String,
    limits: Limits,
    engine: Option<Engine>,
    executor: tokio::runtime::Runtime,
    deadline: Deadline,
}

impl Worker {
    pub(crate) fn new(id: &str, source: String, limits: Limits) -> Result<Self, Error> {
        if source.len() > bounds::SOURCE_BYTES {
            return Err(Error::Invalid("invalid source"));
        }
        if !(bounds::MIN_HEAP_BYTES..=bounds::MAX_HEAP_BYTES).contains(&limits.heap_bytes)
            || limits.execution.is_zero()
            || limits.execution > bounds::MAX_EXECUTION
        {
            return Err(Error::Invalid("limits outside local execution budget"));
        }
        let executor = tokio::runtime::Builder::new_current_thread().build()?;
        let deadline = Deadline::new()?;
        let specifier = format!("chunk:deployment/{id}");
        let engine = Engine::load(
            &executor,
            &deadline,
            &specifier,
            &source,
            limits,
            &Cancellation::default(),
        )?;
        Ok(Self {
            source,
            specifier,
            limits,
            engine: Some(engine),
            executor,
            deadline,
        })
    }

    pub(crate) fn execute(
        &mut self,
        invocation: Invocation,
        host: Box<dyn ReadHost>,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        if invocation.export.is_empty() || invocation.export.len() > bounds::NAME_BYTES {
            return Err(Error::Invalid("invalid export"));
        }
        let caller = serde_json::to_string(&invocation.caller).map_err(js_error)?;
        let arguments = serde_json::to_string(&invocation.arguments).map_err(js_error)?;
        if caller.len() > bounds::JSON_BYTES || arguments.len() > bounds::JSON_BYTES {
            return Err(Error::Invalid("input exceeds size limit"));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self
            .engine
            .as_ref()
            .is_some_and(|engine| engine.calls >= bounds::RUNTIME_CALLS)
        {
            self.engine = None;
        }
        let mut engine = match self.engine.take() {
            Some(engine) => engine,
            None => Engine::load(
                &self.executor,
                &self.deadline,
                &self.specifier,
                &self.source,
                self.limits,
                cancellation,
            )?,
        };
        let _entered = self.executor.enter();
        let prepared = Prepared {
            export: invocation.export,
            caller,
            arguments,
            timestamp: invocation.timestamp,
            seed: invocation.seed,
            capabilities: Capabilities {
                generation: engine.calls + 1,
                host,
                mode: invocation.mode,
                cancellation: cancellation.clone(),
                writes: BTreeMap::new(),
                calls: 0,
                write_bytes: 0,
            },
        };
        let result = engine.execute(&self.executor, &self.deadline, prepared, self.limits, cancellation);
        if result.is_ok() {
            self.engine = Some(engine);
        }
        result
    }
}

struct Prepared {
    export: String,
    caller: String,
    arguments: String,
    timestamp: i64,
    seed: u64,
    capabilities: Capabilities,
}

struct Engine {
    // Persistent handles must drop before their isolate.
    run: Option<v8::Global<v8::Function>>,
    namespace: Option<v8::Global<v8::Object>>,
    runtime: JsRuntime,
    termination: Termination,
    calls: u32,
}

impl Engine {
    fn execute(
        &mut self,
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        prepared: Prepared,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        self.calls += 1;
        let Prepared {
            export,
            caller,
            arguments,
            capabilities,
            timestamp,
            seed,
        } = prepared;
        crate::profile::begin(&mut self.runtime, timestamp, seed)?;
        self.runtime.op_state().borrow_mut().put(Some(capabilities));
        let result = self.guarded(deadline, limits, cancellation, |engine| {
            executor.block_on(engine.invoke(&export, &caller, &arguments))
        });
        crate::profile::end(&mut self.runtime);
        let capabilities = self
            .runtime
            .op_state()
            .borrow_mut()
            .borrow_mut::<Option<Capabilities>>()
            .take()
            .expect("active invocation");
        let value = result?;
        let writes = capabilities
            .writes
            .into_iter()
            .map(|(key, write)| Write {
                key,
                value: write.value,
            })
            .collect();
        Ok(Execution { value, writes })
    }

    fn load(
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        specifier: &str,
        source: &str,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Self, Error> {
        let _entered = executor.enter();
        let termination = Termination::default();
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![chunk_capabilities::init(), crate::profile::chunk_profile::init()],
            create_params: Some(
                v8::Isolate::create_params()
                    .heap_limits(0, limits.heap_bytes)
                    .array_buffer_allocator(crate::allocator::bounded(limits.heap_bytes, termination.clone())),
            ),
            ..Default::default()
        });
        runtime.op_state().borrow_mut().put(None::<Capabilities>);
        crate::profile::initialize(&mut runtime);
        let heap_signal = termination.clone();
        let handle = runtime.v8_isolate().thread_safe_handle();
        runtime.add_near_heap_limit_callback(move |limit, _| {
            heap_signal.record(Reason::Heap);
            handle.terminate_execution();
            limit + bounds::EMERGENCY_HEAP_BYTES
        });
        let mut engine = Self {
            run: None,
            namespace: None,
            runtime,
            termination,
            calls: 0,
        };
        engine.guarded(deadline, limits, cancellation, |engine| {
            executor.block_on(engine.initialize(specifier, source))
        })?;
        Ok(engine)
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
        let run = self
            .runtime
            .execute_script("chunk:bootstrap", include_str!("bootstrap.js"))
            .map_err(js_error)?;
        {
            deno_core::scope!(scope, &mut self.runtime);
            let run = v8::Local::new(scope, run);
            self.run = Some(v8::Global::new(
                scope,
                v8::Local::<v8::Function>::try_from(run).map_err(js_error)?,
            ));
        }
        let module = self
            .runtime
            .load_main_es_module_from_code(&ModuleSpecifier::parse(specifier).map_err(js_error)?, source.to_owned())
            .await
            .map_err(js_error)?;
        let evaluation = self.runtime.mod_evaluate(module);
        self.runtime
            .with_event_loop_promise(evaluation, PollEventLoopOptions::default())
            .await
            .map_err(js_error)?;
        self.drain().await?;
        self.namespace = Some(self.runtime.get_module_namespace(module).map_err(js_error)?);
        Ok(())
    }

    async fn drain(&mut self) -> Result<(), Error> {
        self.runtime
            .run_event_loop(PollEventLoopOptions::default())
            .await
            .map_err(js_error)
    }

    async fn invoke(&mut self, export: &str, caller: &str, arguments: &str) -> Result<String, Error> {
        let args = {
            deno_core::scope!(scope, &mut self.runtime);
            let namespace = v8::Local::new(scope, self.namespace.as_ref().expect("initialized"));
            let key = v8::String::new(scope, export).ok_or(Error::Heap)?;
            let function = namespace
                .get(scope, key.into())
                .ok_or(Error::Invalid("missing export"))?;
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
        let call = self
            .runtime
            .call_with_args(self.run.as_ref().expect("initialized"), &args);
        let output = self
            .runtime
            .with_event_loop_promise(call, PollEventLoopOptions::default())
            .await
            .map_err(js_error)?;
        self.drain().await?;
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
