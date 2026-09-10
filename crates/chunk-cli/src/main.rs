//! Developer commands for the chunk platform.
use clap::{Parser, Subcommand};
use std::{io, path::PathBuf, process::ExitCode};
mod local;
mod platform;
mod players;
use chunk_service::shutdown_signal;

#[derive(Parser)]
#[command(name = "chunk", version, about = "Build and run Minecraft apps")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Run the local dev server.
    #[command(visible_alias = "local")]
    Dev(local::Options),
    /// Check and bundle TypeScript.
    Build {
        #[arg(default_value = ".")]
        project: PathBuf,
        #[arg(long, default_value = ".chunk/build")]
        output: PathBuf,
    },
    /// Inspect project and app manifests as JSON without building.
    Inspect {
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
        Command::Dev(options) => local::run(options).await,
        Command::Build { project, output } => tokio::task::spawn_blocking(move || {
            cliclack::log::info("Building…")?;
            chunk_build::compile(&project, &output)?;
            cliclack::log::success(format!("Built → {}", output.display()))
        })
        .await
        .map_err(io::Error::other)?,
        Command::Inspect { project } => {
            let metadata = chunk_build::project::inspect(&project)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            serde_json::to_writer_pretty(&mut output, &metadata).map_err(io::Error::other)?;
            io::Write::write_all(&mut output, b"\n")
        }
        Command::Upload { .. } => platform::unsupported("Asset uploads"),
        Command::Auth(auth) => platform::auth(auth),
        Command::Login(options) => platform::auth(platform::Auth::Login(options)),
        Command::Deploy(_) => platform::unsupported("Deployments"),
        Command::Logs(_) => platform::unsupported("Logs"),
        Command::Deployments(_) => platform::unsupported("Deployment listing"),
        Command::Environments(_) => platform::unsupported("Environment listing"),
        Command::Apps(_) => platform::unsupported("App listing"),
        Command::Players(options) => players::run(options).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_structure_is_valid() {
        Cli::command().debug_assert();
        assert!(Cli::try_parse_from(["chunk", "auth", "login", "--cloud", "--url", "https://example.com"]).is_err());
    }
}
