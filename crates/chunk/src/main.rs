//! The `chunk` CLI, currently serving the edge player listener.

use std::{io, net::SocketAddr, num::NonZeroUsize, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "The chunk Minecraft platform")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve Minecraft status and online-mode login (sessions are not yet available).
    Edge {
        #[arg(long, default_value = "127.0.0.1:25565")]
        bind: SocketAddr,
        #[arg(long, default_value = "chunk — sessions coming soon")]
        motd: String,
        #[arg(long, default_value = "1024")]
        max_connections: NonZeroUsize,
        /// Enable the dashboard using built static assets from this directory.
        #[arg(long, requires = "management_token")]
        dashboard_dir: Option<PathBuf>,
        #[arg(long, default_value = "127.0.0.1:8080")]
        management_bind: SocketAddr,
        /// Operator token for the management API. Use TLS for remote access.
        #[arg(long, env = "CHUNK_MANAGEMENT_TOKEN", hide_env_values = true)]
        management_token: Option<String>,
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
            dashboard_dir,
            management_bind,
            management_token,
        } => {
            let config = chunk_edge::ProxyConfig {
                motd,
                max_connections,
                ..Default::default()
            };
            let Some(dashboard_dir) = dashboard_dir else {
                return chunk_edge::run(bind, config, shutdown_signal()?).await;
            };
            let token = management_token
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "management token required"))?;
            let cancellation = CancellationToken::new();
            let edge_shutdown = cancellation.clone();
            let management_shutdown = cancellation.clone();
            let servers = async {
                tokio::try_join!(
                    chunk_edge::run(bind, config, async move {
                        edge_shutdown.cancelled().await;
                        Ok(())
                    }),
                    chunk_management::run(
                        chunk_management::Config {
                            bind: management_bind,
                            dashboard_dir,
                            token,
                        },
                        async move {
                            management_shutdown.cancelled().await;
                        }
                    ),
                )?;
                Ok(())
            };
            tokio::pin!(servers);
            tokio::select! {
                result = &mut servers => result,
                result = shutdown_signal()? => {
                    cancellation.cancel();
                    result?;
                    servers.await
                }
            }
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
