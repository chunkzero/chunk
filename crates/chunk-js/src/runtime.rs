use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};

use crate::{
    Cancellation, Error, Execution, Invocation, Limits, ReadHost, Write,
    capabilities::{Capabilities, chunk_capabilities},
    deadline::Deadline,
};

const MAX_CALLS: u32 = 10_000;

pub(crate) struct Worker {
    source: String,
    limits: Limits,
    engine: Option<Engine>,
    executor: tokio::runtime::Runtime,
    deadline: Deadline,
}

impl Worker {
    pub(crate) fn new(source: String, limits: Limits) -> Result<Self, Error> {
        if source.len() > 4 * 1024 * 1024 {
            return Err(Error::Invalid("invalid source"));
        }
        if !(8 * 1024 * 1024..=128 * 1024 * 1024).contains(&limits.heap_bytes)
            || limits.execution.is_zero()
            || limits.execution.as_secs() > 30
        {
            return Err(Error::Invalid("limits outside local execution budget"));
        }
        let executor = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
        let deadline = Deadline::new()?;
        let engine = Engine::load(&executor, &deadline, &source, limits, &Cancellation::default())?;
        Ok(Self {
            source,
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
        if invocation.export.is_empty() || invocation.export.len() > 128 {
            return Err(Error::Invalid("invalid export"));
        }
        let caller = serde_json::to_string(&invocation.caller).map_err(js_error)?;
        let arguments = serde_json::to_string(&invocation.arguments).map_err(js_error)?;
        if caller.len() > 1024 * 1024 || arguments.len() > 1024 * 1024 {
            return Err(Error::Invalid("input exceeds size limit"));
        }
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.engine.as_ref().is_some_and(|engine| engine.calls >= MAX_CALLS) {
            self.engine = None;
        }
        let mut engine = match self.engine.take() {
            Some(engine) => engine,
            None => Engine::load(&self.executor, &self.deadline, &self.source, self.limits, cancellation)?,
        };
        let _entered = self.executor.enter();
        let prepared = Prepared {
            export: invocation.export,
            caller,
            arguments,
            capabilities: Capabilities {
                generation: engine.calls + 1,
                host,
                mode: invocation.mode,
                cancellation: cancellation.clone(),
                writes: BTreeMap::new(),
                calls: 0,
                write_bytes: BTreeMap::new(),
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
    capabilities: Capabilities,
}

struct Engine {
    // Persistent handles must drop before their isolate.
    run: Option<v8::Global<v8::Function>>,
    namespace: Option<v8::Global<v8::Object>>,
    runtime: JsRuntime,
    heap_exhausted: Arc<AtomicBool>,
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
        } = prepared;
        self.runtime.op_state().borrow_mut().put(Some(capabilities));
        let result = self.guarded(deadline, limits, cancellation, |engine| {
            executor.block_on(engine.invoke(&export, &caller, &arguments, limits.execution))
        });
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
            .map(|(key, value)| Write { key, value })
            .collect();
        Ok(Execution { value, writes })
    }

    fn load(
        executor: &tokio::runtime::Runtime,
        deadline: &Deadline,
        source: &str,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Self, Error> {
        let _entered = executor.enter();
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![chunk_capabilities::init()],
            create_params: Some(v8::Isolate::create_params().heap_limits(0, limits.heap_bytes)),
            ..Default::default()
        });
        runtime.op_state().borrow_mut().put(None::<Capabilities>);
        let heap_exhausted = Arc::new(AtomicBool::new(false));
        let heap_signal = Arc::clone(&heap_exhausted);
        let handle = runtime.v8_isolate().thread_safe_handle();
        runtime.add_near_heap_limit_callback(move |limit, _| {
            heap_signal.store(true, Ordering::Release);
            handle.terminate_execution();
            limit + 8 * 1024 * 1024
        });
        let mut engine = Self {
            run: None,
            namespace: None,
            runtime,
            heap_exhausted,
            calls: 0,
        };
        engine.guarded(deadline, limits, cancellation, |engine| {
            executor.block_on(engine.initialize(source, limits.execution))
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
        let started = Instant::now();
        let guard = deadline.arm(handle, cancellation.clone(), limits.execution);
        let result = run(self);
        drop(guard);
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if self.heap_exhausted.load(Ordering::Acquire) {
            return Err(Error::Heap);
        }
        if started.elapsed() >= limits.execution {
            return Err(Error::Deadline);
        }
        result
    }

    async fn initialize(&mut self, source: &str, duration: std::time::Duration) -> Result<(), Error> {
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
            .load_main_es_module_from_code(
                &ModuleSpecifier::parse("chunk:deployment").map_err(js_error)?,
                source.to_owned(),
            )
            .await
            .map_err(js_error)?;
        let evaluation = self.runtime.mod_evaluate(module);
        tokio::time::timeout(
            duration,
            self.runtime
                .with_event_loop_promise(evaluation, PollEventLoopOptions::default()),
        )
        .await
        .map_err(|_| Error::Deadline)?
        .map_err(js_error)?;
        self.drain(duration).await?;
        self.namespace = Some(self.runtime.get_module_namespace(module).map_err(js_error)?);
        Ok(())
    }

    async fn drain(&mut self, duration: std::time::Duration) -> Result<(), Error> {
        tokio::time::timeout(duration, self.runtime.run_event_loop(PollEventLoopOptions::default()))
            .await
            .map_err(|_| Error::Deadline)?
            .map_err(js_error)
    }

    async fn invoke(
        &mut self,
        export: &str,
        caller: &str,
        arguments: &str,
        duration: std::time::Duration,
    ) -> Result<String, Error> {
        let args = {
            deno_core::scope!(scope, &mut self.runtime);
            let namespace = v8::Local::new(scope, self.namespace.as_ref().expect("initialized"));
            let key = v8::String::new(scope, export).ok_or(Error::Heap)?;
            let function = namespace
                .get(scope, key.into())
                .ok_or(Error::Invalid("missing export"))?;
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
        let output = tokio::time::timeout(
            duration,
            self.runtime
                .with_event_loop_promise(call, PollEventLoopOptions::default()),
        )
        .await
        .map_err(|_| Error::Deadline)?
        .map_err(js_error)?;
        self.drain(duration).await?;
        deno_core::scope!(scope, &mut self.runtime);
        let output = v8::Local::new(scope, output);
        let output = v8::Local::<v8::String>::try_from(output).map_err(js_error)?;
        let encoded = output.to_rust_string_lossy(scope);
        if encoded.len() > 1024 * 1024 {
            return Err(Error::Invalid("result exceeds size limit"));
        }
        Ok(encoded)
    }
}

fn js_error(error: impl std::fmt::Display) -> Error {
    Error::JavaScript(error.to_string())
}
