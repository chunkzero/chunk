use super::{Tasks, scope::Origin};
use crate::server::transport::invalid_data;
use chunk_proto::sync::v1::{CommandEffect, command_effect::Effect};
use chunk_protocol::{
    commands::{ActionBar, PlainText, SubtitleText, SystemMessage, TitleText},
    encode_packet,
    versions::v26_2::TitleTimes,
};
use std::io;

/// Writes `effect` to the player while the claim it's for, or with `follow` their connection, is current.
pub(super) async fn render(tasks: &Tasks, origin: &Origin, follow: bool, effect: &CommandEffect) -> io::Result<()> {
    let current = tasks.current(origin, follow)?;
    current.check(&tasks.platform).await?;
    let text = |text: &str| PlainText::new(text.to_owned()).map_err(invalid_data);
    let packets = match effect.effect.as_ref().ok_or_else(|| invalid_data("missing command effect"))? {
        Effect::Message(message) => {
            vec![encode_packet(&SystemMessage { text: text(message)?, overlay: false }).map_err(invalid_data)?]
        }
        Effect::ActionBar(message) => vec![encode_packet(&ActionBar { text: text(message)? }).map_err(invalid_data)?],
        Effect::Title(title) => vec![
            encode_packet(&TitleTimes { fade_in: 10, stay: 70, fade_out: 20 }).map_err(invalid_data)?,
            encode_packet(&SubtitleText { text: text(title.subtitle.as_deref().unwrap_or_default())? })
                .map_err(invalid_data)?,
            encode_packet(&TitleText { text: text(&title.title)? }).map_err(invalid_data)?,
        ],
    };
    tasks.packets(origin, follow, packets).await
}
