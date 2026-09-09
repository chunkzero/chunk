//! The `chunk` CLI, currently serving the edge player listener.

use std::{io, net::SocketAddr, num::NonZeroUsize, path::PathBuf, process::ExitCode};

mod backend;
mod control;
mod players;
mod runtime;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "The chunk Minecraft platform")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the environment backend with an immutable JavaScript deployment bundle.
    Backend(backend::Options),
    /// Serve durable local session placement and player ownership.
    Control(control::Options),
    /// Move a connected player or drain their current runtime.
    Players(players::Options),
    /// Launch a supervised gameplay JVM. Requires Java 25 and the runtime distribution.
    Runtime(runtime::Options),
    /// Serve Minecraft status and online-mode login (sessions are not yet available).
    Edge {
        #[arg(long, default_value = "127.0.0.1:25565")]
        bind: SocketAddr,
        #[arg(long, default_value = "chunk — sessions coming soon")]
        motd: String,
        #[arg(long, requires = "process_token")]
        gameplay: Option<String>,
        #[arg(long, env = "CHUNK_PROCESS_TOKEN", hide_env_values = true)]
        process_token: Option<String>,
        #[arg(long, env = "CHUNK_ENVIRONMENT", default_value = "local")]
        environment: String,
        #[arg(long, env = "CHUNK_DEPLOYMENT", default_value = "local")]
        deployment: String,
        #[arg(long, conflicts_with = "gameplay")]
        runtime_file: Option<PathBuf>,
        #[arg(long, requires = "control_file", conflicts_with_all = ["gameplay", "runtime_file"])]
        backend_file: Option<PathBuf>,
        #[arg(long, requires = "backend_file")]
        control_file: Option<PathBuf>,
        #[arg(long, default_value = "1024")]
        max_connections: NonZeroUsize,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "chunk stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> io::Result<()> {
    match cli.command {
        Command::Backend(options) => backend::run(options).await,
        Command::Control(options) => control::run(options).await,
        Command::Players(options) => players::run(options).await,
        Command::Runtime(options) => runtime::run(options).await,
        Command::Edge {
            bind,
            motd,
            max_connections,
            gameplay,
            runtime_file,
            process_token,
            environment,
            deployment,
            backend_file,
            control_file,
        } => {
            let gameplay = if let Some(path) = runtime_file {
                Some(runtime::read_target(&path)?)
            } else {
                gameplay.map(|endpoint| chunk_edge::GameplayTarget {
                    endpoint,
                    token: process_token.expect("clap requires a process token for gameplay"),
                    environment,
                    deployment,
                })
            };

            let platform = backend_file
                .zip(control_file)
                .map(|(backend, control)| -> io::Result<_> {
                    Ok(chunk_edge::PlatformTarget {
                        backend: serde_json::from_slice(&std::fs::read(backend)?).map_err(io::Error::other)?,
                        control: serde_json::from_slice(&std::fs::read(control)?).map_err(io::Error::other)?,
                    })
                })
                .transpose()?;
            let config = chunk_edge::ProxyConfig {
                platform,
                gameplay,
                motd,
                max_connections,
                ..Default::default()
            };
            chunk_edge::run(bind, config, shutdown_signal()?).await
        }
    }
}

fn shutdown_signal() -> io::Result<impl std::future::Future<Output = io::Result<()>>> {
    #[cfg(unix)]
    let wait = {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = terminate.recv() => {}
            }
        }
    };
    #[cfg(windows)]
    let wait = {
        let mut interrupt = tokio::signal::windows::ctrl_c()?;
        async move {
            interrupt.recv().await;
        }
    };
    Ok(async move {
        wait.await;
        tracing::info!("shutting down");
        Ok(())
    })
}
