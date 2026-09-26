use super::{Commands, Output, Tasks, backend, run, scope::Origin};
use crate::server::transport::invalid_data;
use chunk_proto::v1::{CommandSuggestionRequest, PrepareCommand};
use chunk_protocol::{
    Decode, Packet, VarInt,
    commands::{CommandSuggestions, PlainText, SignedCommand, SystemMessage},
    decode_packet, encode_packet,
    versions::v26_2::{CommandSuggestionsRequest, UnsignedCommand},
};
use std::{io, time::Duration};

impl Commands {
    /// Returns true for every owned root, including hidden, invalid and signed input.
    pub fn input(&self, frame: &[u8]) -> io::Result<bool> {
        let Some(catalog) = &self.catalog else {
            return Ok(false);
        };
        let id = VarInt::decode(&mut &*frame).map_err(invalid_data)?.0;
        let (input, signed, suggestion) = if id == UnsignedCommand::ID {
            (decode_packet::<UnsignedCommand>(frame).map_err(invalid_data)?.command.as_str().to_owned(), false, None)
        } else if id == SignedCommand::ID {
            (decode_packet::<SignedCommand>(frame).map_err(invalid_data)?.command.as_str().to_owned(), true, None)
        } else if id == CommandSuggestionsRequest::ID {
            let packet = decode_packet::<CommandSuggestionsRequest>(frame).map_err(invalid_data)?;
            (packet.text.as_str().to_owned(), false, Some(packet.transaction_id.0))
        } else {
            return Ok(false);
        };
        let Some((id, command)) = catalog.owner(&input) else {
            return Ok(false);
        };
        let Some(origin) = self.origin.clone() else {
            return Ok(true);
        };
        if signed {
            self.tasks.error(&origin, "This command accepts unsigned arguments only.");
            return Ok(true);
        }
        if !self.tree_received || self.tasks.current(&origin, false).is_err() {
            self.tasks.error(&origin, "Commands are available after session arrival.");
            return Ok(true);
        }
        if suggestion.is_none() && catalog.parse(&input).is_err() {
            self.tasks.error(&origin, "Invalid command arguments.");
            return Ok(true);
        }
        let Ok(permit) = self.tasks.capacity.clone().try_acquire_owned() else {
            self.tasks.error(&origin, "Too many pending commands.");
            return Ok(true);
        };
        let tasks = self.tasks.clone();
        let id = id.to_owned();
        let follow = command.follow_player;
        let descriptors = self.descriptors.clone();
        let catalog = catalog.clone();
        tasks.platform.cleanup.clone().spawn(async move {
            let _permit = permit;
            let work = async {
                tasks.current(&origin, false)?;
                origin.check(&tasks.platform).await?;
                let allowed = backend::catalog(&tasks.platform, &origin.scope, &descriptors).await?;
                if let Some(transaction_id) = suggestion {
                    let cursor = u32::try_from(input.encode_utf16().count()).map_err(invalid_data)?;
                    let plan = catalog.suggestions(&input, cursor, |id,_| allowed.contains(id)).map_err(invalid_data)?;
                    let response = if let Some(plan) = plan {
                        let values = if let Some(query) = &plan.query {
                            backend::client(&tasks.platform).suggest(backend::authenticated(&tasks.platform, CommandSuggestionRequest {
                                scope: Some(origin.scope.clone()), command_id: id.clone(), query: query.clone(), input: input.clone(), cursor,
                            })?).await.map_err(io::Error::other)?.into_inner().values
                        } else { Vec::new() };
                        plan.finish(transaction_id, &values).map_err(invalid_data)?
                    } else { CommandSuggestions { transaction_id, start: cursor, length: 0, matches: Vec::new() } };
                    tasks.packets(&origin, false, vec![encode_packet(&response).map_err(invalid_data)?]).await?;
                    return Ok(());
                }
                if !allowed.contains(&id) { return Err(invalid_data("command permission denied")); }
                tasks.current(&origin, false)?;
                origin.check(&tasks.platform).await?;
                let prepared = backend::client(&tasks.platform).prepare(backend::authenticated(&tasks.platform, PrepareCommand {
                    scope: Some(origin.scope.clone()), command_id: id, input,
                })?).await.map_err(io::Error::other)?.into_inner();
                if prepared.follow_player != follow || prepared.invocation_id.is_empty() {
                    return Err(invalid_data("command preparation mismatch"));
                }
                run::execute(&tasks, &origin, follow, &prepared.invocation_id).await
            };
            let cancellation = if follow && suggestion.is_none() { &tasks.connection } else { &origin.cancellation };
            tokio::select! {
                () = tasks.connection.cancelled() => {},
                () = cancellation.cancelled() => {},
                result = tokio::time::timeout(Duration::from_secs(60), work) => {
                    if !matches!(result, Ok(Ok(()))) { tasks.error(&origin, "Command unavailable or outcome unknown."); }
                }
            }
        });
        Ok(true)
    }
}
impl Tasks {
    fn error(&self, origin: &Origin, text: &str) {
        let Ok(text) = PlainText::new(text) else {
            return;
        };
        let Ok(packet) = encode_packet(&SystemMessage { text, overlay: false }) else {
            return;
        };
        let _ = self.output.try_send(Output::Packets {
            origin: Box::new(origin.clone()),
            invocation: self.invocation.clone(),
            follow: false,
            packets: vec![packet],
            acknowledgment: None,
        });
    }
    pub(super) async fn packets(&self, origin: &Origin, follow: bool, packets: Vec<Vec<u8>>) -> io::Result<()> {
        let (acknowledgment, accepted) = tokio::sync::oneshot::channel();
        self.output
            .send(Output::Packets {
                origin: Box::new(origin.clone()),
                invocation: self.invocation.clone(),
                follow,
                packets,
                acknowledgment: Some(acknowledgment),
            })
            .await
            .map_err(io::Error::other)?;
        accepted.await.map_err(io::Error::other)?
    }
}
