//! The `chunk` CLI, currently serving the edge player listener.

use std::{io, net::SocketAddr, num::NonZeroUsize, process::ExitCode};

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
    /// Serve Minecraft status and login rejection (sessions are not yet available).
    Edge {
        #[arg(long, default_value = "127.0.0.1:25565")]
        bind: SocketAddr,
        #[arg(long, default_value = "chunk — sessions coming soon")]
        motd: String,
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
        Command::Edge {
            bind,
            motd,
            max_connections,
        } => {
            let config = chunk_edge::ProxyConfig {
                motd,
                max_connections,
                ..Default::default()
            };
            chunk_edge::run(bind, config, shutdown_signal()).await
        }
    }
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result?,
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await?;
    tracing::info!("shutting down");
    Ok(())
}
