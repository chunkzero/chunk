use super::{Tasks, effects, scope::Origin};
use crate::server::{platform::CommandUpdate, transport::invalid_data};
use chunk_proto::sync::v1::{CommandArguments, CommandOutcome, command_outcome};
use std::{collections::BTreeSet, io};

/// Runs the command `arguments` names for `origin`'s player until its outcome, rendering and acknowledging each effect
/// core holds for it. Dropping the future cancels the command.
pub(super) async fn execute(
    tasks: &Tasks,
    origin: &Origin,
    follow: bool,
    arguments: CommandArguments,
) -> io::Result<()> {
    let platform = &tasks.platform;
    let mut topic = platform.start_command(&origin.player, &arguments).await?;
    let mut rendered = BTreeSet::new();
    loop {
        match topic.next().await? {
            CommandUpdate::Effects(effects) => {
                for (sequence, effect) in effects {
                    if rendered.insert(sequence) {
                        let failed = effects::render(tasks, origin, follow, &effect).await.is_err();
                        platform.acknowledge(&topic, sequence, failed).await?;
                    }
                }
            }
            CommandUpdate::Outcome(CommandOutcome { outcome: Some(command_outcome::Outcome::ResultJson(_)) }) => {
                return Ok(());
            }
            CommandUpdate::Outcome(_) => return Err(invalid_data("command failed or outcome unknown")),
        }
    }
}
