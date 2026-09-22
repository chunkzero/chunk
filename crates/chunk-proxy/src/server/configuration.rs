use std::{future::Future, io, time::Duration};

use chunk_protocol::{
    Decode, Encode, Packet, VarInt, decode_packet,
    versions::v26_2::{
        AcknowledgeConfiguration, ConfigurationClientInformation, ConfigurationClientInformationParticleStatus,
        ConfigurationKeepAlive, ConfigurationKeepAliveResponse, ConfigurationPluginResponse, KnownPacks,
        PlayClientInformation, PlayClientInformationParticleStatus,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until},
};

use super::{
    authentication::Authenticated,
    transport::{PreparedPackets, Transport, WRITE_TIMEOUT, invalid_data, timed_out, within},
};

pub(super) const FRAME_LIMIT: usize = 65536;
const CLIENT_INFORMATION_TIMEOUT: Duration = Duration::from_secs(10);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(15);

/// At most one outstanding keepalive; the next is due an interval after the last acknowledgment.
pub(super) struct KeepAlive {
    next_id: i64,
    pending: Option<(i64, Instant)>,
    due: Instant,
}

impl KeepAlive {
    pub fn new() -> Self {
        Self { next_id: 0, pending: None, due: Instant::now() }
    }

    pub fn idle(&self) -> bool {
        self.pending.is_none()
    }

    /// When the next keepalive is due or, while one is outstanding, when it expires.
    pub fn deadline(&self) -> Instant {
        self.pending.map_or(self.due, |(_, expires)| expires)
    }

    /// At the deadline, returns the ID to send next, or `None` when the outstanding keepalive expired.
    pub fn start(&mut self) -> Option<i64> {
        if self.pending.is_some() {
            return None;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending = Some((id, Instant::now() + KEEP_ALIVE_TIMEOUT));
        Some(id)
    }

    /// Accepts only the outstanding ID.
    pub fn acknowledge(&mut self, id: i64) -> bool {
        if self.pending.is_none_or(|(pending, _)| pending != id) {
            return false;
        }
        self.pending = None;
        self.due = Instant::now() + KEEP_ALIVE_INTERVAL;
        true
    }
}

/// Decodes play-state client settings as the configuration-state packet a destination expects.
pub(super) fn play_settings(frame: &[u8]) -> io::Result<ConfigurationClientInformation> {
    let information = decode_packet::<PlayClientInformation>(frame).map_err(invalid_data)?;
    Ok(ConfigurationClientInformation {
        locale: information.locale,
        view_distance: information.view_distance,
        chat_flags: information.chat_flags,
        chat_colors: information.chat_colors,
        skin_parts: information.skin_parts,
        main_hand: information.main_hand,
        enable_text_filtering: information.enable_text_filtering,
        enable_server_listing: information.enable_server_listing,
        particle_status: match information.particle_status {
            PlayClientInformationParticleStatus::All => ConfigurationClientInformationParticleStatus::All,
            PlayClientInformationParticleStatus::Decreased => ConfigurationClientInformationParticleStatus::Decreased,
            PlayClientInformationParticleStatus::Minimal => ConfigurationClientInformationParticleStatus::Minimal,
        },
    })
}

/// Keeps the client in configuration until a destination is ready. The caller
/// receives the authenticated transport and latest client settings, with no
/// outstanding keepalive, and owns the remaining configuration exchange.
/// Cancelling this future drops the connection and destination future.
pub(super) async fn wait_for_destination<S, T>(
    mut authenticated: Authenticated<S>,
    destination: impl Future<Output = io::Result<T>>,
    max_wait: Duration,
) -> io::Result<(Authenticated<S>, ConfigurationClientInformation, T)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::pin!(destination);
    let started = Instant::now();
    let expires = started + max_wait;
    let information_deadline = started + CLIENT_INFORMATION_TIMEOUT;
    let mut information = None;
    let mut ready = None;
    let mut keep_alive = KeepAlive::new();

    loop {
        if keep_alive.idle()
            && let Some(settings) = information.take()
        {
            if let Some(destination) = ready.take() {
                return Ok((authenticated, settings, destination));
            }
            information = Some(settings);
        }
        tokio::select! {
            biased;
            () = sleep_until(expires) => {
                let _ = disconnect(&mut authenticated.transport, 0x02, "Server temporarily unavailable").await;
                return Err(timed_out("configuration wait expired"));
            },
            () = sleep_until(information_deadline), if information.is_none() => {
                return Err(timed_out("client information timed out"));
            }
            result = &mut destination, if ready.is_none() => match result {
                Ok(value) => ready = Some(value),
                Err(error) => {
                    let reason = if error.kind() == io::ErrorKind::PermissionDenied { error.to_string() } else { "Server temporarily unavailable".into() };
                    let _ = disconnect(&mut authenticated.transport, 0x02, &reason).await;
                    return Err(error);
                }
            },
            () = sleep_until(keep_alive.deadline()) => {
                let id = keep_alive.start().ok_or_else(|| timed_out("configuration keepalive timed out"))?;
                // Never cancel a partially written encrypted packet and then reuse the stream.
                within(WRITE_TIMEOUT.min(expires.saturating_duration_since(Instant::now())), authenticated.transport.write_packet(
                    &ConfigurationKeepAlive { keep_alive_id: id },
                )).await?;
            }
            frame = authenticated.transport.read_frame(FRAME_LIMIT) => {
                let frame = frame?;
                match packet_id(&frame)? {
                    ConfigurationClientInformation::ID => {
                        information = Some(decode_packet::<ConfigurationClientInformation>(&frame).map_err(invalid_data)?);
                    }
                    ConfigurationPluginResponse::ID => {
                        // Brand and channel announcements do not need a response while parked.
                        decode_packet::<ConfigurationPluginResponse>(&frame).map_err(invalid_data)?;
                    }
                    ConfigurationKeepAliveResponse::ID => {
                        let response = decode_packet::<ConfigurationKeepAliveResponse>(&frame).map_err(invalid_data)?;
                        if !keep_alive.acknowledge(response.keep_alive_id) {
                            return Err(invalid_data("unexpected configuration keepalive response"));
                        }
                    }
                    _ => return Err(invalid_data("unexpected packet while waiting in configuration")),
                }
            }
        }
    }
}

pub(super) async fn disconnect<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    packet: i32,
    reason: &str,
) -> io::Result<()> {
    // Anonymous NBT string uses Java modified UTF-8, including surrogate pairs.
    let mut text = Vec::new();
    for unit in reason.chars().take(256).collect::<String>().encode_utf16() {
        match unit {
            1..=127 => text.push(u8::try_from(unit).map_err(invalid_data)?),
            0..=2047 => {
                text.push(0xc0 | u8::try_from(unit >> 6).map_err(invalid_data)?);
                text.push(0x80 | u8::try_from(unit & 63).map_err(invalid_data)?);
            }
            _ => {
                text.push(0xe0 | u8::try_from(unit >> 12).map_err(invalid_data)?);
                text.push(0x80 | u8::try_from((unit >> 6) & 63).map_err(invalid_data)?);
                text.push(0x80 | u8::try_from(unit & 63).map_err(invalid_data)?);
            }
        }
    }
    let mut body = Vec::new();
    VarInt(packet).encode(&mut body).map_err(invalid_data)?;
    body.push(8);
    body.extend(u16::try_from(text.len()).map_err(invalid_data)?.to_be_bytes());
    body.extend(text);
    within(WRITE_TIMEOUT, transport.write_body(&body)).await
}

pub(super) async fn finish<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    settings: &mut ConfigurationClientInformation,
    known_packs: &PreparedPackets,
    registry_and_finish: &PreparedPackets,
) -> io::Result<()> {
    within(WRITE_TIMEOUT, transport.write_prepared(known_packs)).await?;
    loop {
        let frame = transport.read_frame(FRAME_LIMIT).await?;
        match packet_id(&frame)? {
            KnownPacks::ID => {
                if !decode_packet::<KnownPacks>(&frame).map_err(invalid_data)?.packs.as_slice().is_empty() {
                    return Err(invalid_data("client selected an unoffered pack"));
                }
                break;
            }
            _ => configuration_message(&frame, settings)?,
        }
    }
    within(WRITE_TIMEOUT, transport.write_prepared(registry_and_finish)).await?;
    loop {
        let frame = transport.read_frame(FRAME_LIMIT).await?;
        if packet_id(&frame)? == AcknowledgeConfiguration::ID {
            decode_packet::<AcknowledgeConfiguration>(&frame).map_err(invalid_data)?;
            return Ok(());
        }
        configuration_message(&frame, settings)?;
    }
}

fn configuration_message(frame: &[u8], settings: &mut ConfigurationClientInformation) -> io::Result<()> {
    match packet_id(frame)? {
        ConfigurationClientInformation::ID => *settings = decode_packet(frame).map_err(invalid_data)?,
        ConfigurationPluginResponse::ID => {
            decode_packet::<ConfigurationPluginResponse>(frame).map_err(invalid_data)?;
        }
        _ => return Err(invalid_data("unexpected configuration packet")),
    }
    Ok(())
}

pub(super) fn packet_id(mut frame: &[u8]) -> io::Result<i32> {
    Ok(VarInt::decode(&mut frame).map_err(invalid_data)?.0)
}

/// Relays the destination's configuration before allowing play traffic.
pub(super) async fn relay<S, D>(
    client: &mut Transport<S>,
    destination: &mut Transport<D>,
    settings: &mut ConfigurationClientInformation,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    D: AsyncRead + AsyncWrite + Unpin,
{
    use chunk_protocol::versions::v26_2::FinishConfiguration;
    let mut finishing = false;
    loop {
        tokio::select! {
            frame = destination.read_frame(chunk_protocol::MAX_FRAME_SIZE) => {
                let frame = frame?;
                if packet_id(&frame)? == FinishConfiguration::ID {
                    decode_packet::<FinishConfiguration>(&frame).map_err(invalid_data)?;
                    finishing = true;
                }
                within(WRITE_TIMEOUT, client.write_body(&frame)).await?;
            }
            frame = client.read_frame(FRAME_LIMIT) => {
                let frame = frame?;
                if packet_id(&frame)? == ConfigurationClientInformation::ID {
                    *settings = decode_packet(&frame).map_err(invalid_data)?;
                }
                let acknowledged = packet_id(&frame)? == AcknowledgeConfiguration::ID;
                if acknowledged {
                    if !finishing { return Err(invalid_data("premature configuration acknowledgment")); }
                    decode_packet::<AcknowledgeConfiguration>(&frame).map_err(invalid_data)?;
                }
                within(WRITE_TIMEOUT, destination.write_body(&frame)).await?;
                if acknowledged { return Ok(()); }
            }
        }
    }
}

#[cfg(test)]
mod tests;
