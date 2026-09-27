//! A player's commands, which core runs: the catalog and suggestions their session sees, and commands started under a
//! prepared operation ID, whose `command/<op>` topic carries the effects this gateway renders, then the outcome.

use std::{collections::BTreeMap, io, sync::Arc, time::Duration};

use chunk_proto::sync::v1::{
    CallRequest, Caller, CommandArguments, CommandEffect, CommandOutcome, CommandStarted, CommandSubscription,
    CommandsResult, EffectArguments, EffectResult, SubscribeRequest, SuggestArguments, SuggestResult, Update,
    entry::State, error::Code,
};
use prost::Message;
use tonic::Streaming;

use super::{
    Platform, RPC_TIMEOUT, invalid_data,
    sync::{Connection, Failure},
};

/// How long core may take to admit a command, which queues while the backend is busy.
const START_TIMEOUT: Duration = Duration::from_secs(45);

impl Platform {
    /// The commands `player`'s session declares, and the IDs of those they may run.
    pub(in crate::server) async fn commands(&self, player: &str) -> io::Result<CommandsResult> {
        self.player_call("commands", String::new(), player, &(), RPC_TIMEOUT).await
    }

    /// The values query `arguments` suggests to `player`.
    pub(in crate::server) async fn suggest(
        &self,
        player: &str,
        arguments: &SuggestArguments,
    ) -> io::Result<Vec<String>> {
        let result: SuggestResult = self.player_call("suggest", String::new(), player, arguments, RPC_TIMEOUT).await?;
        Ok(result.values)
    }

    /// Starts the command `arguments` names for `player`, returning its topic. Dropping the topic cancels the command.
    pub(in crate::server) async fn start_command(
        &self,
        player: &str,
        arguments: &CommandArguments,
    ) -> io::Result<CommandTopic> {
        let operation = self.prepare().await?;
        let _: CommandStarted =
            self.player_call("command", operation.clone(), player, arguments, START_TIMEOUT).await?;
        CommandTopic::open(self.sync.clone(), operation, None).await
    }

    /// Acknowledges effect `sequence` of the command `topic` follows, once rendered or `failed`.
    pub(in crate::server) async fn acknowledge(
        &self,
        topic: &CommandTopic,
        sequence: u32,
        failed: bool,
    ) -> io::Result<()> {
        let arguments = EffectArguments { operation_id: topic.operation.clone(), sequence, failed };
        let _: (EffectResult, _) = self.call("effect", "", &arguments, RPC_TIMEOUT).await?;
        Ok(())
    }

    /// Calls platform method `chunk:<method>` under `operation` with `player` as its caller.
    async fn player_call<R: Message + Default>(
        &self,
        method: &str,
        operation: String,
        player: &str,
        arguments: &impl Message,
        timeout: Duration,
    ) -> io::Result<R> {
        let message = CallRequest {
            operation_id: operation,
            method: format!("chunk:{method}"),
            arguments: arguments.encode_to_vec(),
            caller: Some(Caller { player: player.to_owned(), ..Caller::default() }),
            ..CallRequest::default()
        };
        Ok(self.fenced(message, timeout).await?.0)
    }
}

/// What a command's topic holds.
pub(in crate::server) enum CommandUpdate {
    /// The effects waiting for this gateway to render and acknowledge them, by sequence number.
    Effects(BTreeMap<u32, CommandEffect>),
    /// How the command finished, the topic's last update.
    Outcome(CommandOutcome),
}

/// A command's `command/<op>` topic, followed from the gateway's current stream, and from its next one within core's
/// grace period whenever that stream is superseded.
pub(in crate::server) struct CommandTopic {
    sync: Arc<Connection>,
    operation: String,
    stream: String,
    updates: Streaming<Update>,
    entries: BTreeMap<String, Vec<u8>>,
}

impl CommandTopic {
    async fn open(sync: Arc<Connection>, operation: String, stale: Option<&str>) -> io::Result<Self> {
        let stream = sync.stream(stale).await?;
        let request = SubscribeRequest {
            topic: format!("command/{operation}"),
            arguments: CommandSubscription { stream: stream.clone() }.encode_to_vec(),
            ..SubscribeRequest::default()
        };
        let updates = sync.subscribe(request).await?;
        Ok(Self { sync, operation, stream, updates, entries: BTreeMap::new() })
    }

    /// The topic's next complete snapshot.
    pub async fn next(&mut self) -> io::Result<CommandUpdate> {
        loop {
            let update = self.updates.message().await.map_err(io::Error::other)?;
            let update = update.ok_or_else(|| invalid_data("command outcome unknown"))?;
            if let Some(error) = update.error {
                if error.code() == Code::Stopped && self.sync.superseded(&self.stream) {
                    let stale = std::mem::take(&mut self.stream);
                    let operation = std::mem::take(&mut self.operation);
                    *self = Self::open(self.sync.clone(), operation, Some(&stale)).await?;
                    continue;
                }
                return Err(io::Error::other(Failure(error)));
            }
            if update.snapshot {
                self.entries.clear();
            }
            for key in &update.removed {
                self.entries.remove(key);
            }
            for entry in update.upserts {
                let Some(State::Value(value)) = entry.state else {
                    return Err(invalid_data("command entry without a value"));
                };
                self.entries.insert(entry.key, value);
            }
            if !update.continued {
                return self.snapshot();
            }
        }
    }

    fn snapshot(&self) -> io::Result<CommandUpdate> {
        if let Some(outcome) = self.entries.get("outcome") {
            return Ok(CommandUpdate::Outcome(CommandOutcome::decode(outcome.as_slice()).map_err(invalid_data)?));
        }
        let effects = self.entries.iter().map(|(sequence, effect)| {
            Ok((
                sequence.parse().map_err(invalid_data)?,
                CommandEffect::decode(effect.as_slice()).map_err(invalid_data)?,
            ))
        });
        effects.collect::<io::Result<_>>().map(CommandUpdate::Effects)
    }
}
