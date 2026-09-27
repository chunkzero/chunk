use std::{io, path::PathBuf, time::Duration};

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
            tokio::time::timeout(Duration::from_millis(patience), stopped(&core, &drained.host)).await.map_err(
                |_| io::Error::other("drain shutdown remains unresolved; retry with the same operation ID"),
            )??;
            cliclack::log::success("Runtime drained.")?;
        }
    }
    Ok(())
}

/// Follows `nodes` until `host` stopped or is gone.
async fn stopped(core: &Core, host: &str) -> io::Result<()> {
    let mut nodes = core.follow::<Node>("nodes").await?;
    loop {
        if nodes.next().await?.get(host).is_none_or(|node| node.phase() == NodePhase::Stopped) {
            return Ok(());
        }
    }
}

fn now_ms() -> u64 {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    u64::try_from(now.as_millis()).unwrap_or(u64::MAX)
}
