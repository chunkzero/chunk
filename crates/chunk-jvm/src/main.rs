//! `chunk-jvm`: the main process of a remote JVM machine. It asks core what its host runs, fetches that release,
//! checked against the digest core names, and supervises the JVM until it exits.

mod address;
mod aot;
mod cache;
mod config;
mod fetch;
mod launch;
mod memory;
mod supervise;

use chunk_proto::sync::v1::{JvmLaunch, jvm_launch::Aot};
use config::Config;
use rustix::process::Signal;
use std::{fmt::Display, time::Instant};
use tokio::sync::mpsc;

/// Why the runner stops without running Java to its end, and the exit code that reports it.
#[derive(Debug)]
pub(crate) struct Failure {
    pub code: u8,
    pub message: String,
}

impl Failure {
    pub fn env(message: impl Display) -> Self {
        Self { code: 64, message: message.to_string() }
    }
    pub fn verify(message: impl Display) -> Self {
        Self { code: 65, message: message.to_string() }
    }
    pub fn unavailable(message: impl Display) -> Self {
        Self { code: 69, message: message.to_string() }
    }
    pub fn io(message: impl Display) -> Self {
        Self { code: 74, message: message.to_string() }
    }
    pub fn refused(message: impl Display) -> Self {
        Self { code: 77, message: message.to_string() }
    }
    pub fn java(message: impl Display) -> Self {
        Self { code: 78, message: message.to_string() }
    }
}

#[tokio::main]
async fn main() {
    chunk_service::logging();
    let code = match signals() {
        Ok(signals) => match Config::load(|name| std::env::var(name).ok()) {
            Ok(config) => run(config, signals).await,
            Err(failure) => Err(failure),
        },
        Err(error) => Err(Failure::io(format!("cannot handle signals: {error}"))),
    };
    let code = code.unwrap_or_else(|failure| {
        eprintln!("{}", serde_json::json!({ "level": "error", "code": failure.code, "message": failure.message }));
        failure.code.into()
    });
    std::process::exit(code);
}

/// Forwards SIGTERM, SIGINT and SIGQUIT to the returned channel.
fn signals() -> std::io::Result<mpsc::UnboundedReceiver<Signal>> {
    use tokio::signal::unix::{SignalKind, signal};
    let (sender, receiver) = mpsc::unbounded_channel();
    for (kind, forwarded) in [
        (SignalKind::terminate(), Signal::TERM),
        (SignalKind::interrupt(), Signal::INT),
        (SignalKind::quit(), Signal::QUIT),
    ] {
        let mut stream = signal(kind)?;
        let sender = sender.clone();
        tokio::spawn(async move { while stream.recv().await.is_some() && sender.send(forwarded).is_ok() {} });
    }
    Ok(receiver)
}

/// Fetches and starts the release core names for this host, and returns the JVM's exit code. SIGTERM or SIGINT before
/// the JVM starts stops the runner cleanly. A JVM that recorded its run makes its AOT cache once it exits.
pub(crate) async fn run(config: Config, mut signals: mpsc::UnboundedReceiver<Signal>) -> Result<i32, Failure> {
    let core = fetch::Core::new(&config)?;
    let boot = uuid::Uuid::new_v4().to_string();
    let mut jvm = tokio::select! {
        jvm = prepare(&config, &core, &boot) => jvm?,
        () = stopped(&mut signals) => return Ok(0),
    };
    let exit = supervise::run(&mut jvm.command, &mut signals, config.stop_grace).await;
    if let aot::Plan::Record(directory) = &jvm.aot {
        aot::finish(&core, &boot, &jvm, directory.path(), matches!(exit, Ok(0)), &mut signals).await;
    }
    exit
}

/// The first SIGTERM or SIGINT.
async fn stopped(signals: &mut mpsc::UnboundedReceiver<Signal>) {
    loop {
        match signals.recv().await {
            Some(signal) if signal != Signal::QUIT => return,
            Some(_) => {}
            None => std::future::pending().await,
        }
    }
}

/// Launches this host's boot `boot`. A failure once core asked for an AOT cache recording tells core no cache comes.
async fn prepare(config: &Config, core: &fetch::Core, boot: &str) -> Result<launch::Jvm, Failure> {
    let player_address = match config.player_address {
        Some(address) => address,
        None => address::detect(config).await?,
    };
    let cache = cache::Cache::open(&config.cache)?;
    let launch = core.launch(boot, &launch::runtime(&config.java_home)).await?;
    let prepared = install(config, core, boot, &launch, &cache, player_address).await;
    if prepared.is_err() && matches!(launch.aot, Some(Aot::Record(_))) {
        aot::abandon(core, boot).await;
    }
    prepared
}

async fn install(
    config: &Config,
    core: &fetch::Core,
    boot: &str,
    launch: &JvmLaunch,
    cache: &cache::Cache,
    player_address: std::net::IpAddr,
) -> Result<launch::Jvm, Failure> {
    cross_check(config, launch)?;
    tracing::info!(host = %config.host, release = %launch.release_id, app = %launch.app, profile = %launch.profile, "core named the launch");
    let directory = cache.directory(&launch.release_id)?;
    let release = if let Some(release) = cache::cached(&directory, &launch.release_id)? {
        release
    } else {
        tracing::info!(size = launch.archive_size, "downloading the release archive");
        let downloading = Instant::now();
        let mut staging = cache.staging()?;
        core.download(boot, launch, staging.as_file_mut()).await?;
        let installing = Instant::now();
        let release = cache::install(staging, launch, &directory).await?;
        tracing::info!(
            download = ?installing - downloading,
            install = ?installing.elapsed(),
            "installed the release"
        );
        release
    };
    let visible = memory::visible_mib(&config.proc);
    let aot = aot::Plan::prepare(core, boot, launch, cache, &config.work_root, visible, config.cpus).await?;
    launch::prepare(config, launch, &release, &directory, player_address, aot)
}

/// Rejects a launch that disagrees with what the machine's environment expects.
fn cross_check(config: &Config, launch: &JvmLaunch) -> Result<(), Failure> {
    for (name, expected, actual) in [
        ("CHUNK_RELEASE_ID", &config.expected.release, &launch.release_id),
        ("CHUNK_APP_ID", &config.expected.app, &launch.app),
        ("CHUNK_MACHINE_PROFILE", &config.expected.profile, &launch.profile),
    ] {
        if let Some(expected) = expected
            && expected != actual
        {
            return Err(Failure::verify(format!("core launches {actual:?}, but {name} names {expected:?}")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
