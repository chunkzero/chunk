//! Persistent `deno_core` (chunk-js `Engine`) versus a persistent direct V8 context, on the same compiled bundle,
//! snapshot host, heap limit and execution deadline. Each cell runs in its own process.
//!
//! `cargo bench -p chunk-js --bench engines -- <bundle>` where `<bundle>` holds `source.mjs` and `contract.json`
//! (for example the `bundle/` directory of a chunk-bench backend run).
mod direct;
mod host;

use std::{
    fs,
    path::Path,
    process::Command,
    time::{Duration, Instant},
};

use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Json, Key, Limits, Mode};
use direct::Writes;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Call = Box<dyn FnMut(&(Json, Json)) -> std::result::Result<(String, Writes), String>>;

const ENGINES: [&str; 2] = ["deno", "direct"];
const WORKLOADS: [(&str, &str, Mode); 3] = [
    ("load", "shared/players/load", Mode::Query),
    ("top", "shared/leaderboard/top", Mode::Query),
    ("save", "shared/players/save", Mode::Mutation),
];

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).filter(|arg| arg != "--bench").collect();
    match args.as_slice() {
        [flag, engine, workload, bundle] if flag == "--cell" => cell(engine, workload, Path::new(bundle)),
        [bundle] => all(Path::new(bundle)),
        _ => Err("usage: engines <directory with source.mjs and contract.json>".into()),
    }
}

fn setting(name: &str, default: usize) -> Result<usize> {
    Ok(std::env::var(name).map_or(Ok(default), |value| value.parse())?)
}

fn all(bundle: &Path) -> Result<()> {
    println!("| Workload | Engine | Median µs | p99 µs | Mean µs | CPU µs/call | Peak RSS MiB |");
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: |");
    let mut raw = Vec::new();
    for (workload, ..) in WORKLOADS {
        let mut checksums = Vec::new();
        for engine in ENGINES {
            let output =
                Command::new(std::env::current_exe()?).args(["--cell", engine, workload]).arg(bundle).output()?;
            if !output.status.success() {
                return Err(format!("{engine} {workload}: {}", String::from_utf8_lossy(&output.stderr)).into());
            }
            let result: Value = serde_json::from_slice(&output.stdout)?;
            let us = |key: &str| result["wall_us"][key].as_f64().unwrap_or_default();
            println!(
                "| {workload} | {engine} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                us("p50"),
                us("p99"),
                us("mean"),
                result["cpu_us_per_call"].as_f64().unwrap_or_default(),
                result["peak_rss_kib"].as_f64().unwrap_or_default() / 1024.0
            );
            checksums.push(result["checksum"].clone());
            raw.push(result);
        }
        if checksums.windows(2).any(|pair| pair[0] != pair[1]) {
            return Err(format!("{workload}: engines returned different results").into());
        }
    }
    for result in raw {
        eprintln!("{result}");
    }
    Ok(())
}

fn cell(engine: &str, workload: &str, bundle: &Path) -> Result<()> {
    let source = fs::read_to_string(bundle.join("source.mjs"))?;
    let contract: Value = serde_json::from_slice(&fs::read(bundle.join("contract.json"))?)?;
    let &(_, function, mode) = WORKLOADS.iter().find(|(name, ..)| *name == workload).ok_or("unknown workload")?;
    let export = contract["functions"][function]["export"].as_str().ok_or("missing export")?.to_owned();
    let name = export.clone();
    let data = host::seed();
    let inputs: Vec<_> = (0..host::PLAYERS)
        .map(|player| {
            // chunk-bench calls these functions as the CLI, naming the player in the arguments.
            let caller = json!({"kind": "cli"});
            let name = format!("p{player}");
            let arguments = match (mode, function) {
                (Mode::Mutation, _) => {
                    let inventory: Vec<_> = (0..16)
                        .map(|slot| json!({"item": format!("item-{}", (player + slot) % 64), "count": 1 + slot}))
                        .collect();
                    json!({"player": name, "coins": player, "xp": player * 3, "level": 1 + player % 50,
                        "best": player * 7, "inventory": inventory})
                }
                (_, "shared/players/load") => json!({"player": name}),
                _ => json!({}),
            };
            (Json::from(caller), Json::from(arguments))
        })
        .collect();
    Engine::init_platform();
    let mut call: Call = match engine {
        "deno" => {
            let mut engine = Engine::new()?;
            let id = DeploymentId::new("bench")?;
            engine.register(id.clone(), source, Limits::default())?;
            Box::new(move |(caller, arguments)| {
                let invocation = Invocation {
                    export: export.clone(),
                    arguments: arguments.clone(),
                    caller: caller.clone(),
                    mode,
                    timestamp: 1_700_000_000_000,
                    seed: 42,
                };
                let host = Box::new(host::Host(data.clone()));
                let execution = engine.execute(&id, invocation, host, &Cancellation::default());
                execution
                    .map(|execution| {
                        (execution.value, execution.writes.into_iter().map(|write| (write.key, write.value)).collect())
                    })
                    .map_err(|error| error.to_string())
            })
        }
        "direct" => {
            let mut direct = direct::Direct::new(&source)?;
            Box::new(move |(caller, arguments)| {
                let host = Box::new(host::Host(data.clone()));
                direct.execute(&export, caller.as_str(), arguments.as_str(), mode, host)
            })
        }
        _ => return Err("unknown engine".into()),
    };
    let (warmup, calls) = (setting("BENCH_WARMUP", 1000)?, setting("BENCH_CALLS", 8000)?);
    for index in 0..warmup {
        let player = index % inputs.len();
        let (_, writes) = call(&inputs[player])?;
        if writes != expected(mode, player as u64, &inputs[player].1)? {
            return Err(format!("unexpected writes for player {player}: {writes:?}").into());
        }
    }
    let mut checksum = Sha256::new();
    let mut samples = Vec::with_capacity(calls);
    let cpu = process_cpu()?;
    let started = Instant::now();
    for index in warmup..warmup + calls {
        let start = Instant::now();
        let (value, writes) = call(&inputs[index % inputs.len()])?;
        samples.push(start.elapsed());
        if writes.len() != usize::from(mode == Mode::Mutation) {
            return Err(format!("unexpected write count {}", writes.len()).into());
        }
        checksum.update(value.as_bytes());
    }
    let elapsed = started.elapsed();
    let cpu = process_cpu()?.saturating_sub(cpu);
    samples.sort_unstable();
    let quantile = |percent: usize| micros(samples[(samples.len() - 1) * percent / 100]);
    let per_call = |duration| micros(duration) / f64::from(u32::try_from(calls).unwrap_or(u32::MAX));
    let result = json!({
        "engine": engine, "workload": workload, "export": name, "warmup": warmup, "calls": calls,
        "bundle_bytes": fs::metadata(bundle.join("source.mjs"))?.len(),
        "wall_us": {"p50": quantile(50), "p99": quantile(99), "max": quantile(100),
            "mean": per_call(elapsed)},
        "cpu_us_per_call": per_call(cpu),
        "peak_rss_kib": peak_rss_kib(),
        "checksum": format!("{:x}", checksum.finalize()),
    });
    println!("{result}");
    Ok(())
}

/// The `save` mutation patches the seeded profile with its arguments at the fixed invocation time.
fn expected(mode: Mode, player: u64, arguments: &Json) -> Result<Writes> {
    if mode == Mode::Query {
        return Ok(Vec::new());
    }
    let (id, mut document) = host::profile(player);
    let arguments: Value = serde_json::from_str(arguments.as_str())?;
    let fields = document.as_object_mut().ok_or("profile object")?;
    fields.extend(arguments.as_object().ok_or("arguments object")?.clone());
    let best = arguments["best"].as_i64().ok_or("best")?;
    fields.insert("rank".into(), json!(-best));
    fields.insert("lastSeen".into(), json!(1_700_000_000_000_u64));
    fields.insert("saves".into(), json!(1));
    Ok(vec![(Key { table: "profiles".into(), id }, Some(document))])
}

fn micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

/// Process CPU time, including the watchdog and V8 platform threads.
fn process_cpu() -> Result<Duration> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    Ok(Duration::new(u64::try_from(time.tv_sec)?, u32::try_from(time.tv_nsec)?))
}

fn peak_rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    status.lines().find_map(|line| line.strip_prefix("VmHWM:")?.trim().strip_suffix(" kB")?.parse().ok())
}
