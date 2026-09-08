use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};

use crate::model::bounds;
use crate::termination::{Reason, Termination};
use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};
use serde_json::Value;

use crate::{
    Cancellation, Error, Execution, Invocation, Limits, ReadHost, Write,
    capabilities::{Capabilities, chunk_capabilities},
};

pub(crate) struct Worker {
    source: String,
    specifier: String,
    limits: Limits,
    engine: Option<Engine>,
    executor: tokio::runtime::Runtime,
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
        let specifier = format!("chunk:deployment/{id}");
        let engine = Engine::load(&executor, &specifier, &source, limits, &Cancellation::default())?;
        Ok(Self {
            source,
            specifier,
            limits,
            engine: Some(engine),
            executor,
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
        for value in [&invocation.arguments, &invocation.caller] {
            if serde_json::to_vec(value).map_err(js_error)?.len() > bounds::JSON_BYTES {
                return Err(Error::Invalid("input exceeds size limit"));
            }
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
            None => Engine::load(&self.executor, &self.specifier, &self.source, self.limits, cancellation)?,
        };
        let _entered = self.executor.enter();
        let result = engine.execute(&self.executor, invocation, host, self.limits, cancellation);
        if result.is_ok() {
            self.engine = Some(engine);
        }
        result
    }
}

struct Engine {
    // Persistent handles must drop before their isolate.
    factory: Option<v8::Global<v8::Function>>,
    serializer: Option<v8::Global<v8::Function>>,
    namespace: Option<v8::Global<v8::Object>>,
    runtime: JsRuntime,
    termination: Termination,
    calls: u32,
}

impl Engine {
    fn execute(
        &mut self,
        executor: &tokio::runtime::Runtime,
        invocation: Invocation,
        host: Box<dyn ReadHost>,
        limits: Limits,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        self.calls += 1;
        crate::profile::begin(&mut self.runtime, invocation.timestamp, invocation.seed)?;
        self.runtime.op_state().borrow_mut().put(Some(Capabilities {
            generation: self.calls,
            host,
            mode: invocation.mode,
            cancellation: cancellation.clone(),
            writes: BTreeMap::new(),
            calls: 0,
            write_bytes: 0,
        }));
        let result = self.guarded(limits, cancellation, |engine| {
            executor.block_on(engine.invoke(invocation))
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
            factory: None,
            serializer: None,
            namespace: None,
            runtime,
            termination,
            calls: 0,
        };
        engine.guarded(limits, cancellation, |engine| {
            executor.block_on(engine.initialize(specifier, source))
        })?;
        Ok(engine)
    }

    fn guarded<T>(
        &mut self,
        limits: Limits,
        cancellation: &Cancellation,
        run: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let handle = self.runtime.v8_isolate().thread_safe_handle();
        let cancel_signal = cancellation.clone();
        let termination = self.termination.clone();
        let running = Arc::new(Mutex::new(true));
        let active = running.clone();
        let started = Instant::now();
        let (done, stopped) = mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            loop {
                let running = active.lock().unwrap();
                if !*running {
                    return;
                }
                let reason = if cancel_signal.is_cancelled() {
                    Some(Reason::Cancelled)
                } else if started.elapsed() >= limits.execution {
                    Some(Reason::Deadline)
                } else {
                    None
                };
                if let Some(reason) = reason {
                    termination.record(reason);
                    handle.terminate_execution();
                    return;
                }
                drop(running);
                if stopped.recv_timeout(std::time::Duration::from_millis(2)) != Err(mpsc::RecvTimeoutError::Timeout) {
                    return;
                }
            }
        });
        let result = run(self);
        *running.lock().unwrap() = false;
        let _ = done.send(());
        let _ = watchdog.join();
        self.termination.take().map_or(result, Err)
    }

    async fn initialize(&mut self, specifier: &str, source: &str) -> Result<(), Error> {
        let factory = self
            .runtime
            .execute_script("chunk:bootstrap", include_str!("bootstrap.js"))
            .map_err(js_error)?;
        let serializer = self
            .runtime
            .execute_script(
                "chunk:result",
                r"((stringify, finite) => value => stringify(value === undefined ? null : value, (_, item) => {
            if (typeof item === 'undefined' || typeof item === 'function' || typeof item === 'symbol' ||
                (typeof item === 'number' && !finite(item))) throw new Error('Result must be JSON');
            return item;
        }))(JSON.stringify, Number.isFinite)",
            )
            .map_err(js_error)?;
        {
            deno_core::scope!(scope, &mut self.runtime);
            let factory = v8::Local::new(scope, factory);
            let serializer = v8::Local::new(scope, serializer);
            self.factory = Some(v8::Global::new(
                scope,
                v8::Local::<v8::Function>::try_from(factory).map_err(js_error)?,
            ));
            self.serializer = Some(v8::Global::new(
                scope,
                v8::Local::<v8::Function>::try_from(serializer).map_err(js_error)?,
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

    async fn invoke(&mut self, invocation: Invocation) -> Result<Value, Error> {
        let (caller, generation) = {
            deno_core::scope!(scope, &mut self.runtime);
            let caller = deno_core::serde_v8::to_v8(scope, invocation.caller).map_err(js_error)?;
            let generation = v8::Integer::new_from_unsigned(scope, self.calls);
            (
                v8::Global::new(scope, caller),
                v8::Global::new(scope, v8::Local::<v8::Value>::from(generation)),
            )
        };
        let context = self
            .runtime
            .call_with_args(self.factory.as_ref().expect("initialized"), &[caller, generation]);
        let context = self
            .runtime
            .with_event_loop_promise(context, PollEventLoopOptions::default())
            .await
            .map_err(js_error)?;
        let (function, arguments) = {
            deno_core::scope!(scope, &mut self.runtime);
            let namespace = v8::Local::new(scope, self.namespace.as_ref().expect("initialized"));
            let key = v8::String::new(scope, &invocation.export).ok_or(Error::Heap)?;
            let function = namespace
                .get(scope, key.into())
                .ok_or(Error::Invalid("missing export"))?;
            if !function.is_function() {
                return Err(Error::Invalid("missing export"));
            }
            let function = v8::Local::<v8::Function>::try_from(function).map_err(js_error)?;
            let arguments = deno_core::serde_v8::to_v8(scope, invocation.arguments).map_err(js_error)?;
            (v8::Global::new(scope, function), v8::Global::new(scope, arguments))
        };
        let call = self.runtime.call_with_args(&function, &[context, arguments]);
        let result = self
            .runtime
            .with_event_loop_promise(call, PollEventLoopOptions::default())
            .await
            .map_err(js_error)?;
        let output = self
            .runtime
            .call_with_args(self.serializer.as_ref().expect("initialized"), &[result]);
        let output = self
            .runtime
            .with_event_loop_promise(output, PollEventLoopOptions::default())
            .await
            .map_err(js_error)?;
        self.drain().await?;
        deno_core::scope!(scope, &mut self.runtime);
        let output = v8::Local::new(scope, output);
        let encoded: String = deno_core::serde_v8::from_v8(scope, output).map_err(js_error)?;
        if encoded.len() > bounds::JSON_BYTES {
            return Err(Error::Invalid("result exceeds size limit"));
        }
        serde_json::from_str(&encoded).map_err(js_error)
    }
}

fn js_error(error: impl std::fmt::Display) -> Error {
    Error::JavaScript(error.to_string())
}
