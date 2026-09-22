use super::super::super::{platform::request, transport::invalid_data};
use super::{Tasks, scope::Origin, session};
use chunk_contract::Effect;
use chunk_proto::v1::{CommandEffect, MovePlayerRequest, SessionDemand};
use chunk_protocol::{
    commands::{ActionBar, PlainText, SubtitleText, SystemMessage, TitleText},
    encode_packet,
    versions::v26_2::TitleTimes,
};
use serde_json::{Value, json};
use std::io;

pub(super) async fn apply(tasks: &Tasks, origin: &Origin, follow: bool, effect: &CommandEffect) -> io::Result<Value> {
    if effect.request_json.len() > 64 * 1024 {
        return Err(invalid_data("command effect exceeds limit"));
    }
    let effect_request: Effect = serde_json::from_slice(&effect.request_json).map_err(invalid_data)?;
    let current = tasks.current(origin, follow)?;
    current.inspect(&tasks.platform).await?;
    match effect_request {
        Effect::Message { text } => {
            tasks
                .packets(
                    origin,
                    follow,
                    vec![
                        encode_packet(&SystemMessage {
                            text: PlainText::new(text).map_err(invalid_data)?,
                            overlay: false,
                        })
                        .map_err(invalid_data)?,
                    ],
                )
                .await?;
        }
        Effect::ActionBar { text } => {
            tasks
                .packets(
                    origin,
                    follow,
                    vec![
                        encode_packet(&ActionBar { text: PlainText::new(text).map_err(invalid_data)? })
                            .map_err(invalid_data)?,
                    ],
                )
                .await?;
        }
        Effect::Title { title, subtitle } => {
            let packets = vec![
                encode_packet(&TitleTimes { fade_in: 10, stay: 70, fade_out: 20 }).map_err(invalid_data)?,
                encode_packet(&SubtitleText {
                    text: PlainText::new(subtitle.unwrap_or_default()).map_err(invalid_data)?,
                })
                .map_err(invalid_data)?,
                encode_packet(&TitleText { text: PlainText::new(title).map_err(invalid_data)? })
                    .map_err(invalid_data)?,
            ];
            tasks.packets(origin, follow, packets).await?;
        }
        Effect::Enter { destination } => {
            // The atomic control check fences replacement connections and concurrent moves.
            tasks
                .platform
                .control
                .clone()
                .move_player(request(
                    MovePlayerRequest {
                        operation_id: effect.operation_id.clone(),
                        player_id: origin.scope.player_uuid.clone(),
                        demand: Some(SessionDemand {
                            key: destination.key,
                            session_type: destination.session_type,
                            machine_profile: destination.machine_profile,
                        }),
                        expected_source: Some(current.identity),
                        expected_connection_id: current.claim.connection_id,
                    },
                    &tasks.platform.target.control.token,
                )?)
                .await
                .map_err(io::Error::other)?;
        }
        Effect::SessionCall { method, arguments } => {
            return session::invoke(tasks, origin, method, arguments, false).await;
        }
        Effect::SessionSend { method, arguments } => {
            session::invoke(tasks, origin, method, arguments, true).await?;
        }
    }
    Ok(json!({"state":"accepted","operationId":effect.operation_id}))
}
