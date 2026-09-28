//! `chunk-jvm`: the main process of a remote JVM machine. It asks core what its host runs, fetches and verifies that
//! release, and supervises the JVM until it exits.

mod address;
mod cache;
mod config;
mod fetch;
mod launch;
mod supervise;

use chunk_proto::sync::v1::JvmLaunch;
use config::Config;
use rustix::process::Signal;
use std::fmt::Display;
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
/// the JVM starts stops the runner with 128 plus the signal's number.
pub(crate) async fn run(config: Config, mut signals: mpsc::UnboundedReceiver<Signal>) -> Result<i32, Failure> {
    let mut jvm = tokio::select! {
        jvm = prepare(&config) => jvm?,
        signal = stopped(&mut signals) => return Ok(128 + signal.as_raw()),
    };
    supervise::run(&mut jvm.command, &mut signals, config.stop_grace).await
}

/// The first SIGTERM or SIGINT.
async fn stopped(signals: &mut mpsc::UnboundedReceiver<Signal>) -> Signal {
    loop {
        match signals.recv().await {
            Some(signal) if signal != Signal::QUIT => return signal,
            Some(_) => {}
            None => std::future::pending().await,
        }
    }
}

async fn prepare(config: &Config) -> Result<launch::Jvm, Failure> {
    let core = fetch::Core::new(config)?;
    let player_address = match config.player_address {
        Some(address) => address,
        None => address::detect(config).await?,
    };
    let cache = cache::Cache::open(&config.cache)?;
    let boot = uuid::Uuid::new_v4().to_string();
    let launch = core.launch(&boot).await?;
    cross_check(config, &launch)?;
    tracing::info!(release = %launch.release_id, app = %launch.app, profile = %launch.profile, "core named the launch");
    let directory = cache.directory(&launch.release_id)?;
    let release = if let Some(release) = cache::cached(&directory, &launch.release_id)? {
        release
    } else {
        tracing::info!(size = launch.archive_size, "downloading the release archive");
        let mut staging = cache.staging()?;
        core.download(&boot, &launch, staging.as_file_mut()).await?;
        cache::install(staging, &launch, &directory).await?
    };
    launch::prepare(config, &launch, &release, &directory, player_address)
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
