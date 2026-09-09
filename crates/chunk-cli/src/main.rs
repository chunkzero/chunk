//! Developer commands for the chunk platform.
use clap::{Parser, Subcommand};
use std::{io, path::PathBuf, process::ExitCode};
mod local;
mod players;
use chunk_service::shutdown_signal;

#[derive(Parser)]
#[command(name = "chunk", version, about = "Build and develop on the chunk Minecraft platform")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Build and run the local platform with embedded services.
    Local(local::Options),
    /// Type-check and bundle application TypeScript into an immutable backend artifact.
    Build {
        #[arg(default_value = ".")]
        project: PathBuf,
        #[arg(long, default_value = ".chunk/build")]
        output: PathBuf,
    },
    /// Upload built assets to the platform (not implemented yet).
    Upload { artifact: PathBuf },
    /// Authenticate with the platform (not implemented yet).
    Login,
    /// Inspect, move or drain local players.
    Players(players::Options),
}
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    chunk_service::logging();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "chunk failed");
            ExitCode::FAILURE
        }
    }
}
async fn run(cli: Cli) -> io::Result<()> {
    match cli.command {
        Command::Local(options) => local::run(options).await,
        Command::Build { project, output } => tokio::task::spawn_blocking(move || {
            chunk_build::compile(&project, &output)?;
            println!("Built backend in {}", output.display());
            Ok(())
        })
        .await
        .map_err(io::Error::other)?,
        Command::Upload { .. } => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "asset upload is not implemented yet",
        )),
        Command::Login => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "platform authentication is not implemented yet",
        )),
        Command::Players(options) => players::run(options).await,
    }
}
