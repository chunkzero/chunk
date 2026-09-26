mod backend;
mod config;
mod control;
mod fixtures;
mod load;
mod metrics;
mod proxy;
mod resources;
mod target;

use std::{
    collections::VecDeque,
    fs::File,
    io::BufRead,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use serde_json::json;
use tokio::{net::TcpListener, task::JoinSet};
use tokio_util::sync::CancellationToken;

use config::{Config, Scenario};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).with_ansi(false).with_writer(std::io::stderr).init();
    if std::env::args().nth(1).as_deref() == Some("--worker") {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let init: target::Init = serde_json::from_str(&line)?;
        compression_level(&init.config)?;
        return runtime(init.config.target_threads)?.block_on(target::serve(init));
    }
    let config = Config::parse();
    config.validate()?;
    compression_level(&config)?;
    ensure!(!cfg!(debug_assertions), "benchmarks require --release (use just bench)");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().and_then(Path::parent).context("workspace root")?;
    let id = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let output = root.join("target/bench").join(format!("{id}-{}", std::process::id()));
    std::fs::create_dir_all(&output)?;
    let metadata = resources::metadata(root)?;
    let config = Arc::new(config);
    serde_json::to_writer_pretty(
        File::create(output.join("config.json"))?,
        &json!({"config": config.as_ref(), "resolved_rate":config.rate(), "machine":metadata}),
    )?;
    let result = runtime(config.generator_threads)?.block_on(async {
        tokio::select! {
            result = run(config, &output) => result,
            result = tokio::signal::ctrl_c() => { result?; anyhow::bail!("benchmark interrupted"); }
        }
    });
    if let Err(error) = &result {
        serde_json::to_writer_pretty(
            File::create(output.join("failure.json"))?,
            &json!({"error":format!("{error:#}")}),
        )?;
    }
    eprintln!("Results: {}", output.display());
    result
}

/// Must run before any runtime thread compresses a packet.
fn compression_level(config: &Config) -> Result<()> {
    if let Some(level) = config.compression_level {
        chunk_proxy::benchmark::set_compression_level(level)?;
    }
    Ok(())
}

fn runtime(threads: usize) -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread().worker_threads(threads).enable_all().build()?)
}

async fn run(config: Arc<Config>, output: &Path) -> Result<()> {
    let state = tempfile::tempdir_in(output)?;
    let stop = CancellationToken::new();
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let mut services = JoinSet::new();
    let token = stop.clone();
    let backend = if config.scenario == Scenario::ProxyRelay {
        let config = config.clone();
        services.spawn(async move { proxy::gameplay(listener, &config, token).await });
        address.to_string()
    } else if config.scenario.is_backend() {
        eprintln!("Compiling the benchmark bundle…");
        let bundle = output.to_path_buf();
        tokio::task::spawn_blocking(move || backend::compile(&bundle)).await??.display().to_string()
    } else {
        services.spawn(fixtures::Runtimes::default().serve(listener, token));
        format!("http://{address}")
    };
    let mut target = target::Target::start(&config, backend, state.path(), output).await?;
    let result = tokio::select! {
        result = measure(config, output.to_path_buf(), &mut target) => result,
        result = services.join_next(), if !services.is_empty() => {
            result.context("fixture missing")???;
            anyhow::bail!("fixture stopped unexpectedly");
        }
    };
    let shutdown = target.stop().await;
    stop.cancel();
    while let Some(result) = services.join_next().await {
        result??;
    }
    result?;
    shutdown
}

async fn clients(
    config: &Config,
    target: &target::Target,
) -> Result<(VecDeque<load::Client>, Option<Arc<backend::Fanout>>)> {
    let mut clients = VecDeque::new();
    let mut fanout = None;
    if let Some(connection) = &target.ready.backend {
        eprintln!("Seeding {} player profiles…", config.population);
        backend::seed(connection, config.population).await?;
        if config.scenario == Scenario::BackendFanout {
            eprintln!("Opening {} subscriptions in groups of {}…", config.subscribers, config.group_size);
            fanout = Some(backend::subscribe(connection, config).await?);
        }
        for _ in 0..config.concurrency {
            clients.push_back(load::Client::Backend(backend::Client::connect(connection, fanout.clone()).await?));
        }
    } else if let Some(connection) = &target.ready.control {
        eprintln!("Seeding {} arrived players through control RPCs…", config.population);
        let mut client = control::Client::connect(connection).await?;
        for index in 0..config.population {
            tokio::time::timeout(Duration::from_secs(30), client.arrive(control::claim(u64::from(index))))
                .await?
                .with_context(|| format!("seeding arrived player {} of {}", index + 1, config.population))?;
        }
        client.verify_population(config.population).await?;
        for _ in 0..config.concurrency {
            clients.push_back(load::Client::Control(control::Client::connect(connection).await?));
        }
    } else {
        for _ in 0..config.concurrency {
            let mut client = proxy::Client::connect(&target.ready.endpoint, config).await?;
            tokio::time::timeout(Duration::from_secs(10), client.exchange(u64::MAX)).await??;
            clients.push_back(load::Client::Proxy(client));
        }
    }
    Ok((clients, fanout))
}

async fn measure(config: Arc<Config>, output: PathBuf, target: &mut target::Target) -> Result<()> {
    let (mut clients, fanout) = clients(&config, target).await?;
    eprintln!(
        "Warmup {}s, then {}s at {}/s with {} lanes…",
        config.warmup,
        config.seconds,
        config.rate(),
        config.concurrency
    );
    let (bytes, wire) = if config.scenario == Scenario::ProxyRelay {
        let response = proxy::response(&config);
        let wire = chunk_proxy::benchmark::frame_len(&response, config.compression())?;
        (
            config.request_bytes + response.len() * config.burst,
            Some(json!({"body_bytes": response.len(), "frame_bytes": wire})),
        )
    } else {
        (0, None)
    };
    let warmup = load::run(config.clone(), &mut clients, config.warmup, 0).await?;
    warmup.write(&output, "warmup")?;
    ensure!(warmup.errors.is_empty(), "warmup operations failed: {:?}", warmup.errors);
    target.reset().await?;
    if let Some(fanout) = &fanout {
        fanout.reset();
    }
    let monitor_stop = CancellationToken::new();
    let monitor = resources::Sampler::new(target.pid()?)?.run(monitor_stop.clone());
    let workload = async {
        let result = load::run(
            config.clone(),
            &mut clients,
            config.seconds,
            u64::from(config.warmup) * u64::from(config.rate()),
        )
        .await;
        monitor_stop.cancel();
        result
    };
    let (stats, samples) = tokio::join!(workload, monitor);
    let stats = stats?;
    stats.write(&output, "measured")?;
    let samples = samples?;
    let target_cpu_ms = samples.last().and_then(|sample| sample["target"]["cpu_ms"].as_u64()).unwrap_or_default();
    let result = json!({
        "measurement": stats.summary(config.seconds, bytes),
        "response": wire,
        "target_cpu_us_per_completed": metrics::count(target_cpu_ms * 1000) / metrics::count(stats.completed.max(1)),
        "phases_us": target.report().await?,
        "fanout": fanout.map(|fanout| fanout.summary(&output)).transpose()?,
    });
    let mut report = json!({"schema_version":1, "warmup":warmup.summary(config.warmup, bytes), "resources":samples});
    report.as_object_mut().context("report object")?.extend(result.as_object().context("result object")?.clone());
    serde_json::to_writer_pretty(File::create(output.join("summary.json"))?, &report)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    if stats.errors.is_empty()
        && let Some(connection) = &target.ready.control
    {
        control::Client::connect(connection).await?.verify_population(config.population).await?;
    }
    Ok(())
}
