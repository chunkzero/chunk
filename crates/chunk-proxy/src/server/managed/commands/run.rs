use super::{Tasks, effects, scope::Origin};
use crate::server::{platform::CommandUpdate, transport::invalid_data};
use chunk_proto::sync::v1::{CommandArguments, CommandEffect, CommandOutcome, command_outcome};
use std::{
    collections::{BTreeMap, BTreeSet},
    convert::Infallible,
    io,
};
use tokio::sync::watch;

/// Runs the command `arguments` names for `origin`'s player until its outcome, following its topic while each effect
/// core holds for it is rendered and acknowledged alongside. Dropping the future cancels the command.
pub(super) async fn execute(
    tasks: &Tasks,
    origin: &Origin,
    follow: bool,
    arguments: CommandArguments,
) -> io::Result<()> {
    let mut topic = tasks.platform.start_command(&origin.player, &arguments).await?;
    let (pending, effects) = watch::channel(BTreeMap::new());
    let delivery = deliver(tasks, origin, follow, topic.operation().to_owned(), effects);
    tokio::pin!(delivery);
    loop {
        let update = tokio::select! {
            update = topic.next() => update?,
            result = &mut delivery => return result.map(|never| match never {}),
        };
        match update {
            CommandUpdate::Effects(effects) => _ = pending.send_replace(effects),
            CommandUpdate::Outcome(CommandOutcome { outcome: Some(command_outcome::Outcome::ResultJson(_)) }) => {
                return Ok(());
            }
            CommandUpdate::Outcome(_) => return Err(invalid_data("command failed or outcome unknown")),
        }
    }
}

/// Renders, then acknowledges, each effect `pending` holds for the command under `operation` once, in sequence order.
async fn deliver(
    tasks: &Tasks,
    origin: &Origin,
    follow: bool,
    operation: String,
    mut pending: watch::Receiver<BTreeMap<u32, CommandEffect>>,
) -> io::Result<Infallible> {
    let mut rendered = BTreeSet::new();
    loop {
        let next = {
            let effects = pending.wait_for(|effects| effects.keys().any(|sequence| !rendered.contains(sequence)));
            let effects = effects.await.map_err(io::Error::other)?;
            effects
                .iter()
                .find(|(sequence, _)| !rendered.contains(*sequence))
                .map(|(&sequence, effect)| (sequence, effect.clone()))
        };
        let Some((sequence, effect)) = next else { continue };
        rendered.insert(sequence);
        let failed = effects::render(tasks, origin, follow, &effect).await.is_err();
        tasks.platform.acknowledge(&operation, sequence, failed).await?;
    }
}
