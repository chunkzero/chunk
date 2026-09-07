use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};

use deno_core::{JsRuntime, ModuleSpecifier, PollEventLoopOptions, RuntimeOptions, v8};
use serde_json::Value;

use crate::{
    Cancellation, Error, Execution, Invocation, Limits, ReadHost, Write,
    capabilities::{Capabilities, chunk_capabilities},
};

/// Executes synchronously on the calling thread; backend callers should use their
/// bounded blocking executor. The watchdog interrupts even synchronous JS loops.
/// # Errors
/// Rejects invalid input, unavailable capabilities, unresolved promises, exceptions,
/// cancellation and heap/time limits. Errors never return speculative writes.
pub fn execute(
    invocation: Invocation,
    host: Box<dyn ReadHost>,
    limits: Limits,
    cancellation: &Cancellation,
) -> Result<Execution, Error> {
    if invocation.deployment.is_empty()
        || invocation.deployment.len() > 128
        || invocation.source.len() > 4 * 1024 * 1024
        || invocation.export.is_empty()
        || invocation.export.len() > 128
    {
        return Err(Error::Invalid("invalid deployment, source or export"));
    }
    if !(8 * 1024 * 1024..=128 * 1024 * 1024).contains(&limits.heap_bytes)
        || limits.execution.is_zero()
        || limits.execution.as_secs() > 30
    {
        return Err(Error::Invalid("limits outside local execution budget"));
    }
    for value in [&invocation.arguments, &invocation.caller] {
        if serde_json::to_vec(value).map_err(js_error)?.len() > 1024 * 1024 {
            return Err(Error::Invalid("input exceeds size limit"));
        }
    }
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    let executor = tokio::runtime::Builder::new_current_thread().enable_time().build()?;
    let _entered = executor.enter();
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![chunk_capabilities::init()],
        create_params: Some(v8::Isolate::create_params().heap_limits(0, limits.heap_bytes)),
        ..Default::default()
    });
    runtime.op_state().borrow_mut().put(Capabilities {
        host,
        mode: invocation.mode,
        cancellation: cancellation.clone(),
        writes: BTreeMap::new(),
        calls: 0,
        write_bytes: BTreeMap::new(),
    });
    let heap_exhausted = Arc::new(AtomicBool::new(false));
    let heap_signal = Arc::clone(&heap_exhausted);
    let handle = runtime.v8_isolate().thread_safe_handle();
    runtime.add_near_heap_limit_callback(move |limit, _| {
        heap_signal.store(true, Ordering::Release);
        handle.terminate_execution();
        limit + 8 * 1024 * 1024
    });
    let handle = runtime.v8_isolate().thread_safe_handle();
    let cancel_signal = cancellation.clone();
    let started = Instant::now();
    let (done, stopped) = mpsc::channel();
    let watchdog = std::thread::spawn(move || {
        loop {
            if cancel_signal.is_cancelled() || started.elapsed() >= limits.execution {
                handle.terminate_execution();
                return;
            }
            if stopped.recv_timeout(std::time::Duration::from_millis(2)) != Err(mpsc::RecvTimeoutError::Timeout) {
                return;
            }
        }
    });
    let result = executor.block_on(run(&mut runtime, invocation, limits.execution));
    let _ = done.send(());
    let _ = watchdog.join();
    if cancellation.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if heap_exhausted.load(Ordering::Acquire) {
        return Err(Error::Heap);
    }
    if started.elapsed() >= limits.execution {
        return Err(Error::Deadline);
    }
    let value = result?;
    let writes = std::mem::take(&mut runtime.op_state().borrow_mut().borrow_mut::<Capabilities>().writes)
        .into_iter()
        .map(|(key, value)| Write { key, value })
        .collect();
    Ok(Execution { value, writes })
}

async fn run(runtime: &mut JsRuntime, invocation: Invocation, duration: std::time::Duration) -> Result<Value, Error> {
    let factory = runtime
        .execute_script("chunk:bootstrap", include_str!("bootstrap.js"))
        .map_err(js_error)?;
    let (factory, caller) = {
        deno_core::scope!(scope, runtime);
        let factory = v8::Local::new(scope, factory);
        let factory = v8::Local::<v8::Function>::try_from(factory).map_err(js_error)?;
        let caller = deno_core::serde_v8::to_v8(scope, invocation.caller).map_err(js_error)?;
        (v8::Global::new(scope, factory), v8::Global::new(scope, caller))
    };
    let context = runtime.call_with_args(&factory, &[caller]);
    let context = runtime
        .with_event_loop_promise(context, PollEventLoopOptions::default())
        .await
        .map_err(js_error)?;
    let serializer = runtime
        .execute_script(
            "chunk:result",
            r"((stringify, finite) => value => stringify(value, (_, item) => {
        if (typeof item === 'undefined' || typeof item === 'function' || typeof item === 'symbol' ||
            (typeof item === 'number' && !finite(item))) throw new Error('Result must be JSON');
        return item;
    }))(JSON.stringify, Number.isFinite)",
        )
        .map_err(js_error)?;
    let serializer = {
        deno_core::scope!(scope, runtime);
        let serializer = v8::Local::new(scope, serializer);
        let serializer = v8::Local::<v8::Function>::try_from(serializer).map_err(js_error)?;
        v8::Global::new(scope, serializer)
    };
    let module = runtime
        .load_main_es_module_from_code(
            &ModuleSpecifier::parse("chunk:deployment").map_err(js_error)?,
            invocation.source,
        )
        .await
        .map_err(js_error)?;
    let evaluation = runtime.mod_evaluate(module);
    tokio::time::timeout(
        duration,
        runtime.with_event_loop_promise(evaluation, PollEventLoopOptions::default()),
    )
    .await
    .map_err(|_| Error::Deadline)?
    .map_err(js_error)?;
    let namespace = runtime.get_module_namespace(module).map_err(js_error)?;
    let (function, arguments) = {
        deno_core::scope!(scope, runtime);
        let namespace = v8::Local::new(scope, namespace);
        let key = v8::String::new(scope, &invocation.export).ok_or(Error::Heap)?;
        let function = namespace
            .get(scope, key.into())
            .ok_or(Error::Invalid("missing export"))?;
        let function = v8::Local::<v8::Function>::try_from(function).map_err(js_error)?;
        let arguments = deno_core::serde_v8::to_v8(scope, invocation.arguments).map_err(js_error)?;
        (v8::Global::new(scope, function), v8::Global::new(scope, arguments))
    };
    let call = runtime.call_with_args(&function, &[context, arguments]);
    let result = tokio::time::timeout(
        duration,
        runtime.with_event_loop_promise(call, PollEventLoopOptions::default()),
    )
    .await
    .map_err(|_| Error::Deadline)?
    .map_err(js_error)?;
    let output = runtime.call_with_args(&serializer, &[result]);
    let output = runtime
        .with_event_loop_promise(output, PollEventLoopOptions::default())
        .await
        .map_err(js_error)?;
    deno_core::scope!(scope, runtime);
    let output = v8::Local::new(scope, output);
    let encoded: String = deno_core::serde_v8::from_v8(scope, output).map_err(js_error)?;
    if encoded.len() > 1024 * 1024 {
        return Err(Error::Invalid("result exceeds size limit"));
    }
    serde_json::from_str(&encoded).map_err(js_error)
}

fn js_error(error: impl std::fmt::Display) -> Error {
    Error::JavaScript(error.to_string())
}
