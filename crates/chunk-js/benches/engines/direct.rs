//! A persistent context driven directly through V8, with chunk-js's transactional host boundary: JSON text reads and
//! writes, invocation-local write overlay, controlled time and randomness, a heap limit and a parked watchdog.
use std::{
    collections::BTreeMap,
    ffi::c_void,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chunk_js::{Key, Mode, Read, ReadHost};
use deno_core::v8;
use serde::Deserialize;
use serde_json::Value;

pub type Writes = Vec<(Key, Option<Value>)>;

const HEAP_BYTES: usize = 32 * 1024 * 1024;
const EMERGENCY_HEAP_BYTES: usize = 8 * 1024 * 1024;
const JSON_BYTES: usize = 1024 * 1024;

struct State {
    generation: u32,
    host: Box<dyn ReadHost>,
    mode: Mode,
    writes: BTreeMap<Key, (Option<Value>, usize)>,
    write_bytes: usize,
    calls: usize,
    timestamp: f64,
    random: u64,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum WriteRequest {
    Put { key: Key, value: Value },
    Delete { key: Key },
}

impl State {
    fn charge(&mut self, generation: u32) -> Result<(), String> {
        self.calls += 1;
        if generation != self.generation {
            return Err("Invocation capability expired".into());
        }
        if self.calls > 4096 {
            return Err("Capability budget exhausted".into());
        }
        Ok(())
    }

    fn read(&mut self, generation: u32, request: &str) -> Result<String, String> {
        self.charge(generation)?;
        if request.len() > 4096 {
            return Err("Read request exceeds size limit".into());
        }
        let value = match serde_json::from_str(request).map_err(|error| error.to_string())? {
            Read::Get { table, id } => {
                let key = Key { table, id };
                let base = self.host.get(&key)?;
                self.writes.get(&key).map_or(base, |(write, _)| write.clone()).unwrap_or(Value::Null)
            }
            Read::Scan { table, start, end } => {
                let mut rows: BTreeMap<_, _> =
                    self.host.scan(&table, start.as_deref(), end.as_deref())?.into_iter().collect();
                for (key, (write, _)) in &self.writes {
                    if key.table == table
                        && start.as_deref().is_none_or(|start| key.id.as_str() >= start)
                        && end.as_deref().is_none_or(|end| key.id.as_str() < end)
                    {
                        match write {
                            Some(value) => rows.insert(key.id.clone(), value.clone()),
                            None => rows.remove(&key.id),
                        };
                    }
                }
                serde_json::to_value(rows.into_iter().collect::<Vec<_>>()).map_err(|error| error.to_string())?
            }
            Read::Index { query } => {
                if query.limit == 0 || query.limit > 1024 {
                    return Err("Index result limit must be 1..1024".into());
                }
                let extra = self.writes.keys().filter(|key| key.table == query.table).count();
                let candidates = chunk_contract::IndexQuery { limit: query.limit + extra, ..query.clone() };
                let indexed = self.host.scan_index(&candidates)?;
                let mut rows: BTreeMap<_, _> = indexed.rows.into_iter().collect();
                for (key, (write, _)) in &self.writes {
                    if key.table == query.table {
                        rows.remove(&key.id);
                        if let Some(value) = write
                            && query.matches(&indexed.fields, value)
                        {
                            rows.insert(key.id.clone(), value.clone());
                        }
                    }
                }
                let mut rows: Vec<_> = rows.into_iter().collect();
                rows.sort_by(|a, b| chunk_contract::IndexQuery::compare(&indexed.fields, a, b));
                rows.truncate(query.limit);
                serde_json::to_value(rows).map_err(|error| error.to_string())?
            }
        };
        let encoded = serde_json::to_string(&value).map_err(|error| error.to_string())?;
        if encoded.len() > JSON_BYTES {
            return Err("Read result exceeds size limit".into());
        }
        Ok(encoded)
    }

    fn write(&mut self, generation: u32, request: &str) -> Result<(), String> {
        self.charge(generation)?;
        if self.mode != Mode::Mutation {
            return Err("Query cannot write".into());
        }
        if request.len() > JSON_BYTES {
            return Err("Document size limit exceeded".into());
        }
        let (key, value) = match serde_json::from_str(request).map_err(|error| error.to_string())? {
            WriteRequest::Put { key, value } => (key, Some(value)),
            WriteRequest::Delete { key } => (key, None),
        };
        if key.table.is_empty() || key.table.len() > 64 || key.id.is_empty() || key.id.len() > 256 {
            return Err("Invalid document key".into());
        }
        if self.writes.len() >= 256 && !self.writes.contains_key(&key) {
            return Err("Write count limit exceeded".into());
        }
        let previous = self.writes.get(&key).map_or(0, |(_, bytes)| *bytes);
        if self.write_bytes - previous + request.len() > 8 * 1024 * 1024 {
            return Err("Write byte limit exceeded".into());
        }
        self.write_bytes = self.write_bytes - previous + request.len();
        self.writes.insert(key, (value, request.len()));
        Ok(())
    }
}

fn throw(scope: &mut v8::PinScope<'_, '_>, message: &str) {
    let message = v8::String::new(scope, message).expect("error message");
    let error = v8::Exception::error(scope, message);
    scope.throw_exception(error);
}

#[allow(clippy::needless_pass_by_value)] // V8 passes callback arguments by value.
fn read<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, mut rv: v8::ReturnValue) {
    let generation = args.get(0).uint32_value(scope).unwrap_or_default();
    let request = args.get(1).to_rust_string_lossy(scope);
    let result = scope.get_slot_mut::<State>().ok_or_else(|| "Invocation capability expired".to_owned());
    match result.and_then(|state| state.read(generation, &request)) {
        Ok(text) => rv.set(v8::String::new(scope, &text).expect("read result").into()),
        Err(error) => throw(scope, &error),
    }
}

#[allow(clippy::needless_pass_by_value)] // V8 passes callback arguments by value.
fn write<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _: v8::ReturnValue) {
    let generation = args.get(0).uint32_value(scope).unwrap_or_default();
    let request = args.get(1).to_rust_string_lossy(scope);
    let result = scope.get_slot_mut::<State>().ok_or_else(|| "Invocation capability expired".to_owned());
    if let Err(error) = result.and_then(|state| state.write(generation, &request)) {
        throw(scope, &error);
    }
}

#[allow(clippy::needless_pass_by_value)] // V8 passes callback arguments by value.
fn now<'s>(scope: &mut v8::PinScope<'s, '_>, _: v8::FunctionCallbackArguments<'s>, mut rv: v8::ReturnValue) {
    match scope.get_slot::<State>().map(|state| state.timestamp) {
        Some(timestamp) => rv.set(v8::Number::new(scope, timestamp).into()),
        None => throw(scope, "Invocation time and randomness unavailable during initialization"),
    }
}

#[allow(clippy::cast_precision_loss, clippy::needless_pass_by_value)]
fn random<'s>(scope: &mut v8::PinScope<'s, '_>, _: v8::FunctionCallbackArguments<'s>, mut rv: v8::ReturnValue) {
    let Some(state) = scope.get_slot_mut::<State>() else {
        return throw(scope, "Invocation time and randomness unavailable during initialization");
    };
    // SplitMix64 with the top 53 bits mapped exactly into [0, 1), as in chunk-js.
    state.random = state.random.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut value = state.random;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;
    rv.set(v8::Number::new(scope, (value >> 11) as f64 / 9_007_199_254_740_992.0).into());
}

static ISOLATE: OnceLock<v8::IsolateHandle> = OnceLock::new();

extern "C" fn near_heap_limit(_: *mut c_void, current: usize, _: usize) -> usize {
    if let Some(handle) = ISOLATE.get() {
        handle.terminate_execution();
    }
    current + EMERGENCY_HEAP_BYTES
}

pub struct Direct {
    // Globals drop before their isolate.
    context: v8::Global<v8::Context>,
    run: v8::Global<v8::Function>,
    namespace: v8::Global<v8::Object>,
    isolate: v8::OwnedIsolate,
    platform: v8::SharedRef<v8::Platform>,
    watchdog: Watchdog,
    calls: u32,
}

impl Direct {
    /// Creates one isolate per process on the calling thread and loads the bundle into a persistent context.
    pub fn new(source: &str) -> Result<Self, String> {
        let mut isolate = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, HEAP_BYTES));
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        ISOLATE.set(isolate.thread_safe_handle()).map_err(|_| "one direct isolate per process")?;
        isolate.add_near_heap_limit_callback(near_heap_limit, std::ptr::null_mut());
        let (context, run, namespace) = {
            v8::scope!(let scope, &mut isolate);
            let template = v8::ObjectTemplate::new(scope);
            let functions = [
                ("__chunk_read", v8::FunctionTemplate::new(scope, read)),
                ("__chunk_write", v8::FunctionTemplate::new(scope, write)),
                ("__chunk_now", v8::FunctionTemplate::new(scope, now)),
                ("__chunk_random", v8::FunctionTemplate::new(scope, random)),
            ];
            for (name, function) in functions {
                let name = v8::String::new(scope, name).ok_or("heap")?;
                template.set(name.into(), function.into());
            }
            let context =
                v8::Context::new(scope, v8::ContextOptions { global_template: Some(template), ..Default::default() });
            let scope = &mut v8::ContextScope::new(scope, context);
            let bootstrap = v8::String::new(scope, include_str!("bootstrap.js")).ok_or("heap")?;
            let run = v8::Script::compile(scope, bootstrap, None).and_then(|script| script.run(scope));
            let run = v8::Local::<v8::Function>::try_from(run.ok_or("bootstrap failed")?).map_err(|e| e.to_string())?;
            let text = v8::String::new(scope, source).ok_or("heap")?;
            let name = v8::String::new(scope, "chunk:deployment/bench").ok_or("heap")?;
            let origin = v8::ScriptOrigin::new(scope, name.into(), 0, 0, false, 0, None, false, false, true, None);
            let mut source = v8::script_compiler::Source::new(text, Some(&origin));
            let module = v8::script_compiler::compile_module(scope, &mut source).ok_or("module compilation failed")?;
            if module.instantiate_module(scope, |_, _, _, _| None) != Some(true) {
                return Err("bundle imports are not supported".into());
            }
            let evaluation = module.evaluate(scope).ok_or("module evaluation failed")?;
            scope.perform_microtask_checkpoint();
            let evaluation = v8::Local::<v8::Promise>::try_from(evaluation).map_err(|e| e.to_string())?;
            if evaluation.state() != v8::PromiseState::Fulfilled {
                return Err("module initialization failed".into());
            }
            let namespace = module.get_module_namespace().to_object(scope).ok_or("module namespace")?;
            (v8::Global::new(scope, context), v8::Global::new(scope, run), v8::Global::new(scope, namespace))
        };
        Ok(Self {
            context,
            run,
            namespace,
            isolate,
            platform: v8::V8::get_current_platform(),
            watchdog: Watchdog::new(),
            calls: 0,
        })
    }

    /// Returns the strict JSON result and the buffered writes.
    pub fn execute(
        &mut self,
        export: &str,
        caller: &str,
        arguments: &str,
        mode: Mode,
        host: Box<dyn ReadHost>,
    ) -> Result<(String, Writes), String> {
        self.calls += 1;
        self.isolate.set_slot(State {
            generation: self.calls,
            host,
            mode,
            writes: BTreeMap::new(),
            write_bytes: 0,
            calls: 0,
            timestamp: 1_700_000_000_000.0,
            random: 42,
        });
        let guard = self.watchdog.arm(self.isolate.thread_safe_handle(), Duration::from_secs(1));
        let result = invoke(
            &mut self.isolate,
            &self.context,
            &self.namespace,
            &self.run,
            self.calls,
            [export, caller, arguments],
        );
        // V8 foreground tasks include GC and compilation work.
        while v8::Platform::pump_message_loop(&self.platform, &self.isolate, false) {}
        drop(guard);
        let state = self.isolate.remove_slot::<State>().ok_or("invocation state missing")?;
        let text = result?;
        if text.len() > JSON_BYTES {
            return Err("result exceeds size limit".into());
        }
        serde_json::from_str::<Value>(&text).map_err(|error| error.to_string())?;
        Ok((text, state.writes.into_iter().map(|(key, (value, _))| (key, value)).collect()))
    }
}

fn invoke(
    isolate: &mut v8::OwnedIsolate,
    context: &v8::Global<v8::Context>,
    namespace: &v8::Global<v8::Object>,
    run: &v8::Global<v8::Function>,
    generation: u32,
    [export, caller, arguments]: [&str; 3],
) -> Result<String, String> {
    v8::scope!(let scope, isolate);
    let context = v8::Local::new(scope, context);
    let scope = &mut v8::ContextScope::new(scope, context);
    let namespace = v8::Local::new(scope, namespace);
    let key = v8::String::new(scope, export).ok_or("heap")?;
    let handler = namespace.get(scope, key.into()).filter(|value| value.is_function()).ok_or("missing export")?;
    let caller = v8::String::new(scope, caller).ok_or("heap")?;
    let generation = v8::Integer::new_from_unsigned(scope, generation);
    let arguments = v8::String::new(scope, arguments).ok_or("heap")?;
    let run = v8::Local::new(scope, run);
    let receiver = v8::undefined(scope).into();
    let output = run
        .call(scope, receiver, &[handler, caller.into(), generation.into(), arguments.into()])
        .ok_or("execution terminated")?;
    scope.perform_microtask_checkpoint();
    let promise = v8::Local::<v8::Promise>::try_from(output).map_err(|e| e.to_string())?;
    match promise.state() {
        v8::PromiseState::Fulfilled => Ok(promise.result(scope).to_rust_string_lossy(scope)),
        v8::PromiseState::Rejected => Err(promise.result(scope).to_rust_string_lossy(scope)),
        v8::PromiseState::Pending => Err("a promise with no possible completion".into()),
    }
}

struct Shared {
    origin: Instant,
    at: AtomicU64,
    active: Mutex<Option<v8::IsolateHandle>>,
    stopped: AtomicBool,
}

impl Shared {
    fn now(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_micros()).unwrap_or(u64::MAX)
    }
}

/// chunk-js's watchdog design: sleeps while idle, polls every two milliseconds while armed.
struct Watchdog {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    fn new() -> Self {
        let shared = Arc::new(Shared {
            origin: Instant::now(),
            at: AtomicU64::new(0),
            active: Mutex::new(None),
            stopped: AtomicBool::new(false),
        });
        let watch = Arc::clone(&shared);
        let thread = thread::spawn(move || {
            while !watch.stopped.load(Ordering::Acquire) {
                if watch.at.load(Ordering::Acquire) == 0 {
                    thread::park();
                    continue;
                }
                thread::park_timeout(Duration::from_millis(2));
                let active = watch.active.lock().expect("watchdog lock");
                if let Some(handle) = active.as_ref()
                    && watch.now() >= watch.at.load(Ordering::Acquire)
                {
                    handle.terminate_execution();
                    watch.at.store(0, Ordering::Release);
                }
            }
        });
        Self { shared, thread: Some(thread) }
    }

    fn arm(&self, handle: v8::IsolateHandle, budget: Duration) -> Guard<'_> {
        // Publish the deadline before releasing the lock, so an awake watchdog never sees the new handle unarmed.
        let mut active = self.shared.active.lock().expect("watchdog lock");
        *active = Some(handle);
        let micros = u64::try_from(budget.as_micros()).unwrap_or(u64::MAX);
        self.shared.at.store(self.shared.now().saturating_add(micros), Ordering::Release);
        self.thread.as_ref().expect("watchdog thread").thread().unpark();
        drop(active);
        Guard(self)
    }
}

struct Guard<'a>(&'a Watchdog);

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        let mut active = self.0.shared.active.lock().expect("watchdog lock");
        self.0.shared.at.store(0, Ordering::Release);
        active.take();
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
