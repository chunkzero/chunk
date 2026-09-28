use std::{collections::BTreeMap, io, path::PathBuf, time::Duration};

use chunk_proto::sync::v1::{
    DrainArguments, DrainResult, MovePlayerArguments, MovePlayerResult, Node, NodePhase, SessionDemand,
    drain_arguments::Target,
};

use crate::core::Core;

#[derive(clap::Args)]
pub(crate) struct Options {
    #[arg(long, default_value = ".chunk/local/control.json")]
    control_file: PathBuf,
    #[arg(long)]
    player: uuid::Uuid,
    /// Retain this ID when retrying an uncertain command.
    #[arg(long, value_parser = crate::core::parse_operation)]
    operation: Option<uuid::Uuid>,
    #[command(subcommand)]
    action: Action,
}

#[derive(clap::Subcommand)]
enum Action {
    /// Move this player on their existing public connection.
    Move {
        #[arg(long)]
        session_type: String,
        #[arg(long)]
        key: String,
        #[arg(long, default_value = "local")]
        machine_profile: String,
    },
    /// Retire the player's current runtime, moving its players before the deadline.
    Drain {
        #[arg(long, default_value = "60")]
        timeout_seconds: u32,
    },
}

pub(crate) async fn run(options: Options) -> io::Result<()> {
    let core = Core::open(&options.control_file).await?;
    let operation_id = crate::core::operation(options.operation);
    cliclack::log::info(format!("Player operation: {operation_id}"))?;
    let player = options.player.to_string();
    match options.action {
        Action::Move { session_type, key, machine_profile } => {
            let destination = Some(SessionDemand { key, session_type, machine_profile });
            let arguments = MovePlayerArguments { player, destination };
            core.call::<MovePlayerResult>("move_player", &operation_id, &arguments).await?;
            cliclack::log::success("Move queued.")?;
        }
        Action::Drain { timeout_seconds } => {
            let arguments = DrainArguments { target: Some(Target::Player(player)), timeout_seconds };
            let drained: DrainResult = core.call("drain", &operation_id, &arguments).await?;
            let patience = drained.deadline_ms.saturating_add(30_000).saturating_sub(now_ms());
            stopped(&core, &drained.host, Duration::from_millis(patience)).await?;
            cliclack::log::success("Runtime drained.")?;
        }
    }
    Ok(())
}

/// Follows `nodes` until `host` stopped or is gone. Opening the topic and reading its first view take up to the call
/// timeout, however little of `patience` is left, so a retry after the deadline still finds a stopped host; later views
/// take up to `patience`.
async fn stopped(core: &Core, host: &str, patience: Duration) -> io::Result<()> {
    let done = |nodes: &BTreeMap<String, Node>| nodes.get(host).is_none_or(|node| node.phase() == NodePhase::Stopped);
    let first = async {
        let mut nodes = core.follow::<Node>("nodes").await?;
        let stopped = done(nodes.next().await?);
        io::Result::Ok((nodes, stopped))
    };
    let first = tokio::time::timeout(crate::core::TIMEOUT, first).await;
    let (mut nodes, stopped) = first.map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    if stopped {
        return Ok(());
    }
    let later = async {
        while !done(nodes.next().await?) {}
        io::Result::Ok(())
    };
    let unresolved = |_| io::Error::other("drain shutdown remains unresolved; retry with the same operation ID");
    tokio::time::timeout(patience, later).await.map_err(unresolved)?
}

fn now_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
