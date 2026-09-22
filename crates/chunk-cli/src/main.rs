//! Developer commands for the chunk platform.
use clap::{Parser, Subcommand};
use std::{io, path::PathBuf, process::ExitCode};
mod building;
mod creation;
mod generation;
mod local;
mod nodes;
mod platform;
mod players;

#[derive(Parser)]
#[command(name = "chunk", version, about = "Build and run Minecraft apps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Create a Java or Kotlin project using the installed SDK.
    Create(creation::Options),
    /// Run the local dev server.
    #[command(visible_alias = "local")]
    Dev(local::Options),
    /// Build the backend and JVM apps into one portable release archive.
    Build(building::Options),
    /// Compile the backend and generate explicitly selected client sources.
    Gen(generation::Options),
    /// Inspect project and app manifests as JSON without building.
    Inspect {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Generate the schema-aware TypeScript SDK for your editor.
    Codegen {
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Upload assets (coming soon).
    Upload { artifact: PathBuf },
    /// Manage authentication.
    #[command(subcommand)]
    Auth(platform::Auth),
    /// Alias for auth login.
    Login(platform::Login),
    /// Deploy an app (coming soon).
    Deploy(platform::Deploy),
    /// View logs (coming soon).
    Logs(platform::Logs),
    /// List deployments (coming soon).
    #[command(subcommand)]
    Deployments(platform::ListCommand),
    /// List environments (coming soon).
    #[command(subcommand)]
    Environments(platform::ListCommand),
    /// List apps (coming soon).
    #[command(subcommand)]
    Apps(platform::ListCommand),
    /// Move players or drain runtimes.
    Players(players::Options),
    /// Inspect nodes and request shutdown.
    Nodes(nodes::Options),
}
#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let inspection = matches!(&cli.command, Command::Inspect { .. });
    if !inspection {
        chunk_service::logging();
    }
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.kind() == io::ErrorKind::Interrupted => ExitCode::from(130),
        Err(error) => {
            if inspection {
                eprintln!("{error}");
            } else {
                let _ = cliclack::outro_cancel(error);
            }
            ExitCode::FAILURE
        }
    }
}
async fn run(cli: Cli) -> io::Result<()> {
    match cli.command {
        Command::Create(options) => creation::run(&options),
        Command::Dev(options) => local::run(options).await,
        Command::Build(options) => building::run(options).await,
        Command::Gen(options) => {
            tokio::task::spawn_blocking(move || generation::run(options)).await.map_err(io::Error::other)?
        }
        Command::Inspect { project } => {
            let metadata = chunk_build::project::inspect(&project)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            serde_json::to_writer_pretty(&mut output, &metadata).map_err(io::Error::other)?;
            io::Write::write_all(&mut output, b"\n")
        }
        Command::Codegen { project } => tokio::task::spawn_blocking(move || {
            chunk_build::generate_sdk(&project)?;
            cliclack::log::success(format!("Generated SDK → {}", project.join(".chunk").display()))
        })
        .await
        .map_err(io::Error::other)?,
        Command::Upload { .. } => platform::unsupported("Asset uploads"),
        Command::Auth(auth) => platform::auth(auth),
        Command::Login(options) => platform::auth(platform::Auth::Login(options)),
        Command::Deploy(_) => platform::unsupported("Deployments"),
        Command::Logs(_) => platform::unsupported("Logs"),
        Command::Deployments(_) => platform::unsupported("Deployment listing"),
        Command::Environments(_) => platform::unsupported("Environment listing"),
        Command::Apps(_) => platform::unsupported("App listing"),
        Command::Players(options) => players::run(options).await,
        Command::Nodes(options) => nodes::run(options).await,
    }
}

#[cfg(test)]
mod tests;
