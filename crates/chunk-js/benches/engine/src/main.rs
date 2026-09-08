mod direct;
mod sync;

use chunk_js_baseline::{Cancellation, Invocation, Key, Limits, Mode, ReadHost, Write};
use deno_core::v8;
use rustix::time::{ClockId, clock_gettime};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const WINDOW: Duration = Duration::from_millis(80);

#[derive(deno_core::serde::Deserialize)]
#[serde(
    crate = "deno_core::serde",
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum Read {
    Get {
        table: String,
        id: String,
    },
    Scan {
        table: String,
        start: Option<String>,
        end: Option<String>,
    },
}

fn env_count(name: &str, default: usize) -> usize {
    std::env::var(name).ok().map_or(default, |value| value.parse().unwrap())
}

#[derive(Clone, Debug, Default)]
pub struct Deps {
    points: BTreeSet<String>,
    scans: Vec<(String, Option<String>, Option<String>)>,
}

/// In-memory snapshot host with dependency recording and speculative writes.
/// Dependencies live behind a shared handle so the boxed `deno` host can report them.
#[derive(Clone)]
pub struct Snapshot {
    rows: BTreeMap<String, Value>,
    deps: Arc<Mutex<Deps>>,
    writes: Vec<Write>,
}

impl Snapshot {
    fn new(revision: usize) -> Self {
        Self::from_rows(
            (0..20)
                .flat_map(|i| {
                    let value = json!({"score": i + revision % 7, "name": format!("player-{i}"), "online": true});
                    [(i.to_string(), value.clone()), (format!("{i:04}"), value)]
                })
                .collect(),
        )
    }
    fn from_rows(rows: BTreeMap<String, Value>) -> Self {
        Self {
            rows,
            deps: Arc::default(),
            writes: Vec::new(),
        }
    }
    fn deps(&self) -> Deps {
        self.deps.lock().unwrap().clone()
    }
    fn read_value(&mut self, read: Read) -> Value {
        match read {
            Read::Get { table, id } => {
                assert_eq!(table, "players");
                self.deps.lock().unwrap().points.insert(id.clone());
                self.rows.get(&id).cloned().unwrap_or(Value::Null)
            }
            Read::Scan { table, start, end } => {
                let rows = self.scan_rows(table, start, end);
                serde_json::to_value(rows).unwrap()
            }
        }
    }
    fn scan_rows(&mut self, table: String, start: Option<String>, end: Option<String>) -> Vec<(String, Value)> {
        self.deps
            .lock()
            .unwrap()
            .scans
            .push((table, start.clone(), end.clone()));
        self.rows
            .iter()
            .filter(|(id, _)| start.as_ref().is_none_or(|s| *id >= s) && end.as_ref().is_none_or(|e| *id < e))
            .map(|(id, value)| (id.clone(), value.clone()))
            .collect()
    }
    fn write_value(&mut self, key: Key, value: Value) {
        self.writes.push(Write {
            key,
            value: Some(value),
        });
    }
}

#[cfg(not(snapshot_host))]
impl ReadHost for Snapshot {
    fn read(&mut self, read: chunk_js_baseline::Read, _: &BTreeMap<Key, Option<Value>>) -> Result<Value, String> {
        let read = match read {
            chunk_js_baseline::Read::Get { table, id } => Read::Get { table, id },
            chunk_js_baseline::Read::Scan { table, start, end } => Read::Scan { table, start, end },
        };
        Ok(self.read_value(read))
    }
}

#[cfg(snapshot_host)]
impl ReadHost for Snapshot {
    fn get(&mut self, key: &Key) -> Result<Option<Value>, String> {
        assert_eq!(key.table, "players");
        self.deps.lock().unwrap().points.insert(key.id.clone());
        Ok(self.rows.get(&key.id).cloned())
    }
    fn scan(&mut self, table: &str, start: Option<&str>, end: Option<&str>) -> Result<Vec<(String, Value)>, String> {
        Ok(self.scan_rows(table.to_owned(), start.map(str::to_owned), end.map(str::to_owned)))
    }
}

pub struct Job {
    export: String,
    arguments: Value,
    snapshot: Snapshot,
}

/// Engine results either as parsed JSON (legacy paths) or as the JSON text the
/// engine produced (tuned paths, which never parse results on the host).
#[derive(Clone, Debug)]
pub enum Payload {
    Json(Value),
    Text(String),
}

impl From<Value> for Payload {
    fn from(value: Value) -> Self {
        Self::Json(value)
    }
}

impl From<String> for Payload {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl Payload {
    fn value(&self) -> Value {
        match self {
            Self::Json(value) => value.clone(),
            Self::Text(text) => serde_json::from_str(text).unwrap(),
        }
    }
}

impl PartialEq for Payload {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Json(a), Self::Json(b)) => a == b,
            (Self::Text(a), Self::Text(b)) => a == b,
            _ => self.value() == other.value(),
        }
    }
}

pub struct Sample {
    value: Payload,
    phases: [f64; 3],
}

pub struct Outcome {
    sample: Sample,
    deps: Deps,
    writes: Vec<Write>,
}

fn fixture(kib: usize) -> String {
    let mut source = include_str!("fixture.js").to_owned();
    let mut count = 0;
    while source.len() < kib * 1024 {
        source.push_str(&format!("function rule{count}(x) {{ return {{score: x.score + {count}, online: x.online, tag: 'rule-{count}'}}; }}\n"));
        count += 1;
    }
    if count > 0 {
        source.push_str("const rules = [");
        for i in 0..count {
            source.push_str(&format!("rule{i},"));
        }
        source.push_str("];\n");
        if std::env::var("BENCH_INIT").as_deref() != Ok("declarations") {
            source.push_str("const defaults = rules.map(rule => rule({score: 1, online: true}));\n");
            source.push_str("if (defaults.length !== rules.length) throw new Error('invalid fixture');\n");
        }
    }
    source
}

fn cpu_us() -> f64 {
    let time = clock_gettime(ClockId::ProcessCPUTime);
    time.tv_sec as f64 * 1e6 + time.tv_nsec as f64 / 1e3
}

fn summary(values: &[f64]) -> Value {
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    json!({"mean": sorted.iter().sum::<f64>() / sorted.len() as f64,
        "median": sorted[sorted.len() / 2], "p99": sorted[(sorted.len() * 99 / 100).min(sorted.len() - 1)],
        "max": sorted[sorted.len() - 1]})
}

fn cpu_directory() -> (String, bool) {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    if let Some(path) = cgroup.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        fields.next()?;
        fields
            .next()?
            .split(',')
            .any(|controller| controller == "cpu")
            .then(|| fields.next())
            .flatten()
    }) {
        return (format!("/sys/fs/cgroup/cpu,cpuacct{path}"), false);
    }
    let path = cgroup.lines().find_map(|line| line.strip_prefix("0::")).unwrap();
    (format!("/sys/fs/cgroup{path}"), true)
}

fn cpu_max() -> String {
    let (path, v2) = cpu_directory();
    if v2 {
        std::fs::read_to_string(format!("{path}/cpu.max"))
            .unwrap_or_default()
            .trim()
            .into()
    } else {
        let quota = std::fs::read_to_string(format!("{path}/cpu.cfs_quota_us")).unwrap();
        let period = std::fs::read_to_string(format!("{path}/cpu.cfs_period_us")).unwrap();
        format!("{} {}", quota.trim(), period.trim())
    }
}

fn cpu_stat() -> String {
    let (path, _) = cpu_directory();
    std::fs::read_to_string(format!("{path}/cpu.stat")).unwrap_or_default()
}

fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(
        args.len() >= 4,
        "usage: engine-bench deno|fresh|persistent|tuned|actor|isolate|context|cold|cached|terminate empty|reads|query|sync BUNDLE_KIB [BURST_PER_80MS]"
    );
    let warmup = env_count("BENCH_WARMUP", 1_000);
    let calls = env_count("BENCH_CALLS", 10_000);
    let engine = &args[1];
    let export = &args[2];
    let kib = args[3].parse::<usize>().unwrap();
    let burst = args.get(4).map(|x| x.parse::<usize>().unwrap()).unwrap_or(0);
    let source = fixture(kib);
    let bytes = source.len();
    let sync_mode = export == "sync";
    let mut evaluate: Box<dyn FnMut(Job) -> Outcome> = if engine == "deno" {
        #[cfg(not(caller_engine))]
        let mut deployment = chunk_js_baseline::Deployment::new("benchmark".into(), source, Limits::default()).unwrap();
        #[cfg(caller_engine)]
        let (mut deployment, deployment_id) = {
            let mut engine = chunk_js_baseline::Engine::new().unwrap();
            let id = chunk_js_baseline::DeploymentId::new("benchmark").unwrap();
            engine.register(id.clone(), source, Limits::default()).unwrap();
            (engine, id)
        };
        Box::new(move |job: Job| {
            let deps = job.snapshot.deps.clone();
            let result = deployment
                .execute(
                    #[cfg(caller_engine)]
                    &deployment_id,
                    Invocation {
                        export: job.export.clone(),
                        arguments: job.arguments.into(),
                        caller: json!({"id": "benchmark-player"}).into(),
                        #[cfg(invocation_context)]
                        timestamp: 1_700_000_000_000,
                        #[cfg(invocation_context)]
                        seed: 7,
                        mode: if job.export == "bump" {
                            Mode::Mutation
                        } else {
                            Mode::Query
                        },
                    },
                    Box::new(job.snapshot),
                    &Cancellation::default(),
                )
                .unwrap();
            Outcome {
                sample: Sample {
                    value: result.value.into(),
                    phases: [0.0; 3],
                },
                deps: deps.lock().unwrap().clone(),
                writes: result.writes,
            }
        })
    } else {
        v8::V8::initialize_platform(v8::new_default_platform(2, false).make_shared());
        v8::V8::initialize();
        if engine == "fresh" || engine == "persistent" || engine == "tuned" {
            let worker = direct::Worker::new(engine != "fresh", engine == "tuned", source);
            Box::new(move |job| worker.call(job))
        } else if engine == "actor" {
            // The sync engine and the isolate share this thread: no channel hop.
            let mut actor = direct::Actor::new(source);
            Box::new(move |job| actor.call(job))
        } else if engine == "terminate" {
            let termination = direct::Termination::new();
            Box::new(move |_| Outcome {
                sample: termination.call(),
                deps: Deps::default(),
                writes: Vec::new(),
            })
        } else {
            assert!(["isolate", "context", "cold", "cached"].contains(&engine.as_str()));
            let mut primitive = direct::primitive(engine, kib);
            Box::new(move |_| Outcome {
                sample: primitive(),
                deps: Deps::default(),
                writes: Vec::new(),
            })
        }
    };
    // Independent expected results verify both implementations and changing snapshots.
    let expected: Vec<_> = (0..7)
        .map(|revision| match export.as_str() {
            "reads" => json!({"total": 45 + revision * 10}),
            "query" => json!(
                (10..20)
                    .rev()
                    .map(|i| json!({"id": format!("{i:04}"), "score": i + revision}))
                    .collect::<Vec<_>>()
            ),
            "empty" | "sync" => Value::Null,
            _ => panic!("unknown export"),
        })
        .collect();
    let mut sync = sync_mode.then(sync::SyncEngine::new);
    if let Some(sync) = &mut sync {
        sync.subscribe(&mut evaluate);
    }
    let mut run = |i: usize| -> Sample {
        if let Some(sync) = &mut sync {
            let (value, reevaluated) = sync.operate(&mut evaluate, i, &format!("{:04}", i % 20));
            return Sample {
                value,
                phases: [reevaluated as f64, 0.0, 0.0],
            };
        }
        let outcome = evaluate(Job {
            export: export.clone(),
            arguments: json!({}),
            snapshot: Snapshot::new(i),
        });
        assert!(outcome.writes.is_empty());
        outcome.sample
    };
    let check = |i: usize, sample: &Sample| {
        if sync_mode {
            // Mutation n on player p returns p + 1 + previous bumps of p.
            assert_eq!(sample.value.value(), json!(i % 20 + 1 + i / 20), "operation {i}");
            assert_eq!(sample.phases[0], 2.0, "operation {i} re-evaluations");
        } else {
            assert_eq!(sample.value.value(), expected[i % 7]);
        }
    };
    for i in 0..warmup {
        check(i, &run(i));
    }
    let mut wall = Vec::with_capacity(calls);
    let mut cpu = Vec::with_capacity(calls);
    let mut response = Vec::with_capacity(calls);
    let mut phases = [Vec::new(), Vec::new(), Vec::new()];
    let mut windows = Vec::new();
    let stats_before = cpu_stat();
    let cpu_start = cpu_us();
    let begin = Instant::now();
    let mut window_cpu = cpu_start;
    for i in 0..calls {
        let index = if sync_mode { warmup + i } else { i };
        let scheduled = if let Some(window) = i.checked_div(burst) {
            begin + WINDOW * u32::try_from(window).unwrap()
        } else {
            Instant::now()
        };
        if burst > 0 && i % burst == 0 {
            if i > 0 {
                windows.push(cpu_us() - window_cpu);
            }
            if let Some(delay) = scheduled.checked_duration_since(Instant::now()) {
                std::thread::sleep(delay);
            }
            window_cpu = cpu_us();
        }
        let start = Instant::now();
        let cpu_before = cpu_us();
        let result = run(index);
        cpu.push(cpu_us() - cpu_before);
        wall.push(start.elapsed().as_secs_f64() * 1e6);
        response.push(scheduled.elapsed().as_secs_f64() * 1e6);
        check(index, &result);
        for (values, value) in phases.iter_mut().zip(result.phases) {
            values.push(value);
        }
    }
    if burst > 0 {
        windows.push(cpu_us() - window_cpu);
    }
    let elapsed = begin.elapsed().as_secs_f64();
    let process_cpu_us = cpu_us() - cpu_start;
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    println!(
        "{}",
        json!({"engine": engine, "export": export, "bundle_bytes": bytes,
        "baseline_revision": env!("BENCH_BASELINE_REVISION"),
        "cache_stage": std::env::var("BENCH_CACHE").unwrap_or("instantiate".into()),
        "bundle_init": std::env::var("BENCH_INIT").unwrap_or("eager".into()),
        "foreground_tasks_pumped": engine != "deno",
        "v8": v8::V8::get_version(), "calls": calls, "warmup": warmup, "burst_per_80ms": burst,
        "cpu_max": cpu_max(), "cpu_stat_before": stats_before, "cpu_stat_after": cpu_stat(),
        "wall_us": summary(&wall), "call_cpu_us": summary(&cpu), "response_us": summary(&response),
        "process_cpu_us_per_call": process_cpu_us / calls as f64,
        "elapsed_seconds": elapsed, "calls_per_second": calls as f64 / elapsed,
        "context_us": summary(&phases[0]), "bootstrap_us": summary(&phases[1]), "module_us": summary(&phases[2]),
        "termination_us": if engine == "terminate" { summary(&phases[0]) } else { Value::Null },
        "batch_cpu_us": if windows.is_empty() { Value::Null } else { summary(&windows) },
        "sync": sync.as_ref().map_or(Value::Null, sync::SyncEngine::summary),
        "peak_rss": status.lines().find(|line| line.starts_with("VmHWM:")).unwrap_or("") })
    );
}
