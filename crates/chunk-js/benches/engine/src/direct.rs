use std::{
    sync::{
        Arc, LazyLock,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use deno_core::{serde_v8, v8};
use serde_json::Value;

use crate::{Job, Outcome, Payload, Sample, Snapshot, fixture};

static CACHE_AFTER_EVALUATION: LazyLock<bool> =
    LazyLock::new(|| std::env::var("BENCH_CACHE").is_ok_and(|value| value == "evaluate"));

fn read<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, mut rv: v8::ReturnValue) {
    let request: crate::Read = serde_v8::from_v8(scope, args.get(0)).unwrap();
    let value = scope.get_slot_mut::<Snapshot>().unwrap().read_value(request);
    rv.set(serde_v8::to_v8(scope, value).unwrap());
}

fn write<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _: v8::ReturnValue) {
    let key: chunk_js_baseline::Key = serde_v8::from_v8(scope, args.get(0)).unwrap();
    let value: Value = serde_v8::from_v8(scope, args.get(1)).unwrap();
    scope.get_slot_mut::<Snapshot>().unwrap().write_value(key, value);
}

fn optional(scope: &mut v8::PinScope<'_, '_>, value: v8::Local<v8::Value>) -> Option<String> {
    (!value.is_null_or_undefined()).then(|| value.to_rust_string_lossy(scope))
}

// Tuned host boundary: primitive arguments in, JSON text out, no serde_v8 trees.
fn read_json<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, mut rv: v8::ReturnValue) {
    let kind = args.get(0).to_rust_string_lossy(scope);
    let table = args.get(1).to_rust_string_lossy(scope);
    let request = if kind == "get" {
        crate::Read::Get {
            table,
            id: args.get(2).to_rust_string_lossy(scope),
        }
    } else {
        let start = optional(scope, args.get(2));
        let end = optional(scope, args.get(3));
        crate::Read::Scan { table, start, end }
    };
    let value = scope.get_slot_mut::<Snapshot>().unwrap().read_value(request);
    let text = serde_json::to_string(&value).unwrap();
    rv.set(v8::String::new(scope, &text).unwrap().into());
}

fn write_json<'s>(scope: &mut v8::PinScope<'s, '_>, args: v8::FunctionCallbackArguments<'s>, _: v8::ReturnValue) {
    let table = args.get(0).to_rust_string_lossy(scope);
    let id = args.get(1).to_rust_string_lossy(scope);
    let value: Value = serde_json::from_str(&args.get(2).to_rust_string_lossy(scope)).unwrap();
    scope
        .get_slot_mut::<Snapshot>()
        .unwrap()
        .write_value(chunk_js_baseline::Key { table, id }, value);
}

fn compile<'s>(
    scope: &mut v8::PinScope<'s, '_, v8::Context>,
    code: &str,
    cache: Option<&[u8]>,
) -> v8::Local<'s, v8::Module> {
    let text = v8::String::new(scope, code).unwrap();
    let name = v8::String::new(scope, "chunk:deployment/benchmark").unwrap();
    let origin = v8::ScriptOrigin::new(scope, name.into(), 0, 0, false, 0, None, false, false, true, None);
    let mut source = match cache {
        Some(bytes) => {
            v8::script_compiler::Source::new_with_cached_data(text, Some(&origin), v8::CachedData::new(bytes))
        }
        None => v8::script_compiler::Source::new(text, Some(&origin)),
    };
    let module = v8::script_compiler::compile_module2(
        scope,
        &mut source,
        if cache.is_some() {
            v8::script_compiler::CompileOptions::ConsumeCodeCache
        } else {
            v8::script_compiler::CompileOptions::NoCompileOptions
        },
        v8::script_compiler::NoCacheReason::NoReason,
    )
    .unwrap();
    if let Some(data) = source.get_cached_data() {
        assert!(!data.rejected(), "code cache rejected");
    }
    module
}

fn function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    object: v8::Local<v8::Object>,
    name: &str,
) -> v8::Local<'s, v8::Function> {
    let key = v8::String::new(scope, name).unwrap();
    let value = object.get(scope, key.into()).unwrap();
    v8::Local::<v8::Function>::try_from(value).unwrap()
}

struct Loaded {
    tuned: bool,
    context: v8::Global<v8::Context>,
    namespace: v8::Global<v8::Object>,
    factory: v8::Global<v8::Function>,
    arguments: Option<v8::Global<v8::Function>>,
    serializer: v8::Global<v8::Function>,
}

fn load(isolate: &mut v8::OwnedIsolate, source: &str, cache: &mut Option<Vec<u8>>) -> (Loaded, [f64; 3]) {
    load_with(isolate, source, cache, false)
}

fn load_with(
    isolate: &mut v8::OwnedIsolate,
    source: &str,
    cache: &mut Option<Vec<u8>>,
    tuned: bool,
) -> (Loaded, [f64; 3]) {
    v8::scope!(let scope, isolate);
    let start = Instant::now();
    let template = v8::ObjectTemplate::new(scope);
    let name = v8::String::new(scope, "__read").unwrap();
    let callback = if tuned {
        v8::FunctionTemplate::new(scope, read_json)
    } else {
        v8::FunctionTemplate::new(scope, read)
    };
    template.set(name.into(), callback.into());
    let name = v8::String::new(scope, "__write").unwrap();
    let callback = if tuned {
        v8::FunctionTemplate::new(scope, write_json)
    } else {
        v8::FunctionTemplate::new(scope, write)
    };
    template.set(name.into(), callback.into());
    let context = v8::Context::new(
        scope,
        v8::ContextOptions {
            global_template: Some(template),
            ..Default::default()
        },
    );
    let context_us = start.elapsed().as_secs_f64() * 1e6;
    let scope = &mut v8::ContextScope::new(scope, context);
    let start = Instant::now();
    let text = if tuned {
        include_str!("bootstrap-json.js")
    } else {
        include_str!("bootstrap.js")
    };
    let bootstrap = v8::String::new(scope, text).unwrap();
    let script = v8::Script::compile(scope, bootstrap, None).unwrap();
    let bootstrap = script.run(scope).unwrap().to_object(scope).unwrap();
    let factory = function(scope, bootstrap, "context");
    let factory = v8::Global::new(scope, factory);
    let arguments = tuned.then(|| {
        let parse = function(scope, bootstrap, "arguments");
        v8::Global::new(scope, parse)
    });
    let serializer = function(scope, bootstrap, "serialize");
    let serializer = v8::Global::new(scope, serializer);
    let bootstrap_us = start.elapsed().as_secs_f64() * 1e6;
    let start = Instant::now();
    let module = compile(scope, source, cache.as_deref());
    assert_eq!(module.instantiate_module(scope, |_, _, _, _| None), Some(true));
    let unbound = cache.is_none().then(|| module.get_unbound_module_script(scope));
    if !*CACHE_AFTER_EVALUATION && let Some(unbound) = unbound {
        *cache = Some(unbound.create_code_cache().unwrap().to_vec());
    }
    let evaluation = module.evaluate(scope).unwrap();
    scope.perform_microtask_checkpoint();
    let promise = v8::Local::<v8::Promise>::try_from(evaluation).unwrap();
    assert_eq!(promise.state(), v8::PromiseState::Fulfilled);
    if *CACHE_AFTER_EVALUATION && let Some(unbound) = unbound {
        *cache = Some(unbound.create_code_cache().unwrap().to_vec());
    }
    let namespace = module.get_module_namespace().to_object(scope).unwrap();
    let namespace = v8::Global::new(scope, namespace);
    let module_us = start.elapsed().as_secs_f64() * 1e6;
    (
        Loaded {
            tuned,
            context: v8::Global::new(scope, context),
            namespace,
            factory,
            arguments,
            serializer,
        },
        [context_us, bootstrap_us, module_us],
    )
}

const CALLER_JSON: &str = r#"{"id":"benchmark-player"}"#;

fn invoke(isolate: &mut v8::OwnedIsolate, loaded: &Loaded, job: &Job) -> Payload {
    v8::scope!(let scope, isolate);
    let context = v8::Local::new(scope, &loaded.context);
    let scope = &mut v8::ContextScope::new(scope, context);
    let time = v8::Number::new(scope, 1_700_000_000_000.0);
    let seed = v8::Integer::new(scope, 42);
    let receiver = v8::undefined(scope).into();
    let factory = v8::Local::new(scope, &loaded.factory);
    let (ctx, arguments) = if let Some(parse) = &loaded.arguments {
        let caller = v8::String::new(scope, CALLER_JSON).unwrap();
        let ctx = factory
            .call(scope, receiver, &[caller.into(), time.into(), seed.into()])
            .unwrap();
        let text = v8::String::new(scope, &job.arguments.to_string()).unwrap();
        let parse = v8::Local::new(scope, parse);
        (ctx, parse.call(scope, receiver, &[text.into()]).unwrap())
    } else {
        let caller = serde_v8::to_v8(scope, serde_json::json!({"id": "benchmark-player"})).unwrap();
        let ctx = factory
            .call(scope, receiver, &[caller, time.into(), seed.into()])
            .unwrap();
        (ctx, serde_v8::to_v8(scope, job.arguments.clone()).unwrap())
    };
    let namespace = v8::Local::new(scope, &loaded.namespace);
    let handler = function(scope, namespace, &job.export);
    let mut value = handler.call(scope, receiver, &[ctx, arguments]).unwrap();
    scope.perform_microtask_checkpoint();
    if let Ok(promise) = v8::Local::<v8::Promise>::try_from(value) {
        assert_eq!(promise.state(), v8::PromiseState::Fulfilled);
        value = promise.result(scope);
    }
    let serializer = v8::Local::new(scope, &loaded.serializer);
    let serialized = serializer.call(scope, receiver, &[value]).unwrap();
    let text = serialized.to_rust_string_lossy(scope);
    assert!(text.len() <= 1024 * 1024);
    if loaded.tuned {
        Payload::Text(text)
    } else {
        Payload::Json(serde_json::from_str(&text).unwrap())
    }
}

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_ms() -> u64 {
    START.elapsed().as_millis() as u64
}

/// One watchdog thread polls an atomic deadline instead of being woken per call.
struct Deadline {
    at: Arc<AtomicU64>,
}

impl Deadline {
    fn spawn(handle: v8::IsolateHandle) -> Self {
        let at = Arc::new(AtomicU64::new(0));
        let shared = Arc::clone(&at);
        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(5));
                let deadline = shared.load(Ordering::Acquire);
                if deadline != 0 && now_ms() > deadline {
                    handle.terminate_execution();
                    shared.store(0, Ordering::Release);
                }
            }
        });
        Self { at }
    }
    fn arm(&self, budget: Duration) {
        self.at.store(now_ms() + budget.as_millis() as u64, Ordering::Release);
    }
    fn clear(&self) {
        self.at.store(0, Ordering::Release);
    }
}

/// Sync engine and isolate on the caller's thread: the boundary is a function call.
pub struct Actor {
    isolate: v8::OwnedIsolate,
    loaded: Loaded,
    deadline: Deadline,
    platform: v8::SharedRef<v8::Platform>,
}

impl Actor {
    pub fn new(source: String) -> Self {
        let platform = v8::V8::get_current_platform();
        let mut isolate = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, 32 * 1024 * 1024));
        isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
        let deadline = Deadline::spawn(isolate.thread_safe_handle());
        let (loaded, _) = load_with(&mut isolate, &source, &mut None, true);
        Self {
            isolate,
            loaded,
            deadline,
            platform,
        }
    }

    pub fn call(&mut self, mut job: Job) -> Outcome {
        self.deadline.arm(Duration::from_secs(1));
        self.isolate.set_slot(std::mem::replace(
            &mut job.snapshot,
            Snapshot::from_rows(Default::default()),
        ));
        let value = invoke(&mut self.isolate, &self.loaded, &job);
        while v8::Platform::pump_message_loop(&self.platform, &self.isolate, false) {}
        self.deadline.clear();
        let snapshot = self.isolate.remove_slot::<Snapshot>().unwrap();
        Outcome {
            sample: Sample {
                value,
                phases: [0.0; 3],
            },
            deps: snapshot.deps(),
            writes: snapshot.writes,
        }
    }
}

enum Watch {
    Arm(v8::IsolateHandle),
    Done,
}

pub struct Worker {
    sender: Option<mpsc::Sender<Job>>,
    receiver: mpsc::Receiver<Outcome>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Worker {
    pub fn new(persistent: bool, tuned: bool, source: String) -> Self {
        let (sender, jobs) = mpsc::channel::<Job>();
        let (results, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            let platform = v8::V8::get_current_platform();
            let mut isolate = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, 32 * 1024 * 1024));
            isolate.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
            let deadline = tuned.then(|| Deadline::spawn(isolate.thread_safe_handle()));
            let (control, commands) = mpsc::channel();
            let watchdog = thread::spawn(move || {
                while let Ok(command) = commands.recv() {
                    if let Watch::Arm(handle) = command {
                        match commands.recv_timeout(Duration::from_secs(1)) {
                            Ok(Watch::Done) => (),
                            Err(mpsc::RecvTimeoutError::Timeout) => {
                                handle.terminate_execution();
                            }
                            _ => panic!("invalid watchdog sequence"),
                        }
                    }
                }
            });
            let mut cache = None;
            let (initial, _) = load_with(&mut isolate, &source, &mut cache, tuned);
            let initial = if persistent {
                Some(initial)
            } else {
                drop(initial);
                None
            };
            while let Ok(job) = jobs.recv() {
                match &deadline {
                    Some(deadline) => deadline.arm(Duration::from_secs(1)),
                    None => control.send(Watch::Arm(isolate.thread_safe_handle())).unwrap(),
                }
                isolate.set_slot(job.snapshot.clone());
                let (value, phases) = if let Some(loaded) = &initial {
                    (invoke(&mut isolate, loaded, &job), [0.0; 3])
                } else {
                    let (loaded, phases) = load(&mut isolate, &source, &mut cache);
                    (invoke(&mut isolate, &loaded, &job), phases)
                };
                // V8 foreground tasks include GC/JIT work, separate from JS microtasks.
                while v8::Platform::pump_message_loop(&platform, &isolate, false) {}
                match &deadline {
                    Some(deadline) => deadline.clear(),
                    None => control.send(Watch::Done).unwrap(),
                }
                let snapshot = isolate.remove_slot::<Snapshot>().unwrap();
                results
                    .send(Outcome {
                        sample: Sample { value, phases },
                        deps: snapshot.deps(),
                        writes: snapshot.writes,
                    })
                    .unwrap();
            }
            drop(control);
            watchdog.join().unwrap();
        });
        Self {
            sender: Some(sender),
            receiver,
            thread: Some(thread),
        }
    }

    pub fn call(&self, job: Job) -> Outcome {
        self.sender.as_ref().unwrap().send(job).unwrap();
        self.receiver.recv().unwrap()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.sender.take();
        self.thread.take().unwrap().join().unwrap();
    }
}

pub fn primitive(kind: &str, bundle_kib: usize) -> impl FnMut() -> Sample {
    let platform = v8::V8::get_current_platform();
    let mut isolate = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, 32 * 1024 * 1024));
    let source = fixture(bundle_kib);
    let mut cache = None;
    let (loaded, _) = load(&mut isolate, &source, &mut cache);
    let kind = kind.to_owned();
    move || {
        let mut phases = [0.0; 3];
        if kind == "isolate" {
            let isolate = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, 32 * 1024 * 1024));
            drop(isolate);
        } else if kind == "cold" {
            // A new isolate excludes V8's implicit in-memory compilation cache.
            let mut cold = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, 32 * 1024 * 1024));
            cold.set_microtasks_policy(v8::MicrotasksPolicy::Explicit);
            let (loaded, timings) = load(&mut cold, &source, &mut None);
            phases = timings;
            drop(loaded);
        } else {
            v8::scope!(let scope, &mut isolate);
            if kind == "context" {
                std::hint::black_box(v8::Context::new(scope, Default::default()));
            } else {
                let context = v8::Local::new(scope, &loaded.context);
                let scope = &mut v8::ContextScope::new(scope, context);
                let module = compile(scope, &source, if kind == "cached" { cache.as_deref() } else { None });
                assert_eq!(module.instantiate_module(scope, |_, _, _, _| None), Some(true));
                assert!(module.evaluate(scope).is_some());
                scope.perform_microtask_checkpoint();
            }
        }
        while v8::Platform::pump_message_loop(&platform, &isolate, false) {}
        Sample {
            value: Payload::Json(Value::Null),
            phases,
        }
    }
}

struct Entered(mpsc::Sender<v8::IsolateHandle>);

fn entered(scope: &mut v8::PinScope, _: v8::FunctionCallbackArguments, _: v8::ReturnValue) {
    scope
        .get_slot::<Entered>()
        .unwrap()
        .0
        .send(scope.thread_safe_handle())
        .unwrap();
}

pub struct Termination {
    jobs: Option<mpsc::Sender<()>>,
    ready: mpsc::Receiver<v8::IsolateHandle>,
    done: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Termination {
    pub fn new() -> Self {
        let (jobs, receiver) = mpsc::channel();
        let (ready_tx, ready) = mpsc::channel();
        let (done_tx, done) = mpsc::channel();
        let thread = thread::spawn(move || {
            while receiver.recv().is_ok() {
                let mut isolate = v8::Isolate::new(v8::CreateParams::default().heap_limits(0, 32 * 1024 * 1024));
                isolate.set_slot(Entered(ready_tx.clone()));
                {
                    v8::scope!(let scope, &mut isolate);
                    let template = v8::ObjectTemplate::new(scope);
                    let key = v8::String::new(scope, "entered").unwrap();
                    let callback = v8::FunctionTemplate::new(scope, entered);
                    template.set(key.into(), callback.into());
                    let context = v8::Context::new(
                        scope,
                        v8::ContextOptions {
                            global_template: Some(template),
                            ..Default::default()
                        },
                    );
                    let scope = &mut v8::ContextScope::new(scope, context);
                    let code = v8::String::new(scope, "entered(); while (true) {}").unwrap();
                    let script = v8::Script::compile(scope, code, None).unwrap();
                    let caught = std::pin::pin!(v8::TryCatch::new(scope));
                    let caught = caught.init();
                    assert!(script.run(&caught).is_none());
                    assert!(caught.has_terminated());
                }
                done_tx.send(()).unwrap();
                // Never reuse the terminated isolate.
                drop(isolate);
            }
        });
        Self {
            jobs: Some(jobs),
            ready,
            done,
            thread: Some(thread),
        }
    }

    pub fn call(&self) -> Sample {
        self.jobs.as_ref().unwrap().send(()).unwrap();
        let handle = self.ready.recv().unwrap();
        let start = Instant::now();
        assert!(handle.terminate_execution());
        self.done.recv().unwrap();
        Sample {
            value: Payload::Json(Value::Null),
            phases: [start.elapsed().as_secs_f64() * 1e6, 0.0, 0.0],
        }
    }
}

impl Drop for Termination {
    fn drop(&mut self) {
        self.jobs.take();
        self.thread.take().unwrap().join().unwrap();
    }
}
