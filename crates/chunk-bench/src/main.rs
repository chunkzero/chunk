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
        return runtime(init.config.target_threads)?.block_on(target::serve(init));
    }
    let config = Config::parse();
    config.validate()?;
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
    } else {
        services.spawn(fixtures::Runtimes::default().serve(listener, token));
        format!("http://{address}")
    };
    let target = target::Target::start(&config, backend, state.path()).await?;
    let result = tokio::select! {
        result = measure(config, output.to_path_buf(), &target) => result,
        result = services.join_next() => {
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

async fn clients(config: &Config, target: &target::Target) -> Result<VecDeque<load::Client>> {
    let mut clients = VecDeque::new();
    if let Some(connection) = &target.ready.control {
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
    Ok(clients)
}

async fn measure(config: Arc<Config>, output: PathBuf, target: &target::Target) -> Result<()> {
    let mut clients = clients(&config, target).await?;
    eprintln!(
        "Warmup {}s, then {}s at {}/s with {} lanes…",
        config.warmup,
        config.seconds,
        config.rate(),
        config.concurrency
    );
    let bytes = if config.scenario == Scenario::ProxyRelay {
        config.request_bytes + config.response_bytes * config.burst
    } else {
        0
    };
    let warmup = load::run(config.clone(), &mut clients, config.warmup, 0).await?;
    warmup.write(&output, "warmup")?;
    ensure!(warmup.errors.is_empty(), "warmup operations failed: {:?}", warmup.errors);
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
    let summary = stats.summary(config.seconds, bytes);
    let report = json!({"schema_version":1, "warmup":warmup.summary(config.warmup, bytes), "measurement":summary, "resources":samples?});
    serde_json::to_writer_pretty(File::create(output.join("summary.json"))?, &report)?;
    println!("{}", serde_json::to_string_pretty(&summary)?);
    if stats.errors.is_empty()
        && let Some(connection) = &target.ready.control
    {
        control::Client::connect(connection).await?.verify_population(config.population).await?;
    }
    Ok(())
}
