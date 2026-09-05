mod packets;
mod world;

pub(super) use packets::Cache;
use packets::Packets;

use std::{future::Future, io, time::Duration};

use chunk_protocol::{
    Decode, Encode, Packet, VarInt, decode_packet,
    versions::v26_1::{
        AcknowledgeConfiguration, ChunkBatchReceived, ConfigurationClientInformation, ConfigurationPluginResponse,
        ConfirmTeleport, KnownPacks, PlayKeepAlive, PlayKeepAliveResponse, PlayPing, PlayPong, PlayerLoaded,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until, timeout},
};

use super::{
    authentication::Authenticated,
    configuration,
    transport::{PreparedPackets, Transport, invalid_data},
};

const LIMBO_TIMEOUT: Duration = Duration::from_secs(60);

const FRAME_LIMIT: usize = 65536;
const ACK_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);

/// Returns the same authenticated play transport once all outstanding protocol
/// acknowledgments are drained. The caller owns reconfiguration/session handoff.
/// Cancellation drops the transport and destination future.
pub(super) async fn wait_for_destination<S, T>(
    authenticated: Authenticated<S>,
    destination: impl Future<Output = io::Result<T>>,
    configuration_timeout: Duration,
    cache: &Cache,
) -> io::Result<(Authenticated<S>, ConfigurationClientInformation, T)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let packets = cache.get(authenticated.protocol_version)?;
    timeout(LIMBO_TIMEOUT, async {
        let (mut authenticated, mut settings, ()) =
            configuration::wait_for_destination(authenticated, std::future::ready(Ok(())), configuration_timeout)
                .await?;
        timeout(
            configuration_timeout,
            configure(&mut authenticated.transport, &mut settings, packets),
        )
        .await
        .map_err(|_| timed_out("limbo configuration timed out"))??;
        timeout(ACK_TIMEOUT, send_prepared(&mut authenticated.transport, &packets.spawn))
            .await
            .map_err(|_| timed_out("limbo spawn write timed out"))??;
        tracing::info!(
            username = authenticated.profile.username.as_str(),
            "limbo spawn sent; awaiting client acknowledgments"
        );
        play(authenticated, settings, destination, packets).await
    })
    .await
    .map_err(|_| timed_out("limbo waiting limit reached"))?
}

async fn configure<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    settings: &mut ConfigurationClientInformation,
    packets: &Packets,
) -> io::Result<()> {
    send_prepared(transport, &packets.known_packs).await?;
    loop {
        let frame = transport.read_frame(FRAME_LIMIT).await?;
        match packet_id(&frame)? {
            KnownPacks::ID => {
                if !decode_packet::<KnownPacks>(&frame)
                    .map_err(invalid_data)?
                    .packs
                    .as_slice()
                    .is_empty()
                {
                    return Err(invalid_data("client selected an unoffered pack"));
                }
                break;
            }
            _ => configuration_message(&frame, settings)?,
        }
    }
    send_prepared(transport, &packets.configuration).await?;
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
        _ => return Err(invalid_data("unexpected limbo configuration packet")),
    }
    Ok(())
}

async fn play<S, T>(
    mut authenticated: Authenticated<S>,
    mut settings: ConfigurationClientInformation,
    destination: impl Future<Output = io::Result<T>>,
    packets: &Packets,
) -> io::Result<(Authenticated<S>, ConfigurationClientInformation, T)>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    tokio::pin!(destination);
    let mut ready = None;
    let mut keep_alive = None;
    let mut next_keep_alive = Instant::now();
    let mut next_id = 0_i64;
    let mut teleport = Some(Instant::now() + ACK_TIMEOUT);
    let loading_deadline = Instant::now() + ACK_TIMEOUT;
    let mut batch_received = false;
    let mut loaded = false;
    loop {
        if keep_alive.is_none()
            && teleport.is_none()
            && batch_received
            && loaded
            && let Some(destination) = ready.take()
        {
            return Ok((authenticated, settings, destination));
        }
        let heartbeat_deadline = keep_alive.map_or(next_keep_alive, |(_, deadline)| deadline);
        tokio::select! {
            biased;
            () = sleep_until(teleport.unwrap_or(loading_deadline)), if teleport.is_some() => return Err(timed_out("teleport acknowledgment timed out")),
            () = sleep_until(loading_deadline), if !batch_received || !loaded => return Err(timed_out("limbo loading timed out")),
            result = &mut destination, if ready.is_none() => ready = Some(result?),
            () = sleep_until(heartbeat_deadline) => {
                if keep_alive.is_some() { return Err(timed_out("play keepalive timed out")); }
                send(&mut authenticated.transport, &PlayKeepAlive { keep_alive_id: next_id }).await?;
                keep_alive = Some((next_id, Instant::now() + ACK_TIMEOUT));
                next_id = next_id.wrapping_add(1);
            }
            frame = authenticated.transport.read_frame(FRAME_LIMIT) => {
                let frame = frame?;
                match packet_id(&frame)? {
                    PlayKeepAliveResponse::ID => {
                        let id = decode_packet::<PlayKeepAliveResponse>(&frame).map_err(invalid_data)?.keep_alive_id;
                        if keep_alive.is_none_or(|(pending, _)| pending != id) { return Err(invalid_data("unexpected play keepalive")); }
                        keep_alive = None;
                        next_keep_alive = Instant::now() + KEEP_ALIVE_INTERVAL;
                    }
                    ConfirmTeleport::ID => {
                        let id = decode_packet::<ConfirmTeleport>(&frame).map_err(invalid_data)?.teleport_id.0;
                        if teleport.is_none() || id != 0 { return Err(invalid_data("unexpected teleport acknowledgment")); }
                        teleport = None;
                    }
                    ChunkBatchReceived::ID => {
                        let rate = decode_packet::<ChunkBatchReceived>(&frame).map_err(invalid_data)?.chunks_per_tick;
                        if batch_received || !rate.is_finite() || rate <= 0.0 { return Err(invalid_data("invalid chunk batch acknowledgment")); }
                        batch_received = true;
                    }
                    PlayerLoaded::ID => {
                        decode_packet::<PlayerLoaded>(&frame).map_err(invalid_data)?;
                        if !loaded {
                            send_prepared(&mut authenticated.transport, &packets.title).await?;
                            loaded = true;
                        }
                    }
                    PlayPing::ID => {
                        let ping = decode_packet::<PlayPing>(&frame).map_err(invalid_data)?;
                        send(&mut authenticated.transport, &PlayPong { id: ping.id }).await?;
                    }
                    0x0e => {
                        // The settings body is shared between configuration and play.
                        let mut body = frame.as_ref();
                        VarInt::decode(&mut body).map_err(invalid_data)?;
                        settings = ConfigurationClientInformation::decode(&mut body).map_err(invalid_data)?;
                        if !body.is_empty() { return Err(invalid_data("trailing client settings")); }
                    }
                    id if (0..=0x44).contains(&id) && id != 0x10 => {} // Bounded chat, inventory and input packets have no effect in limbo.
                    _ => return Err(invalid_data("unknown play packet")),
                }
            }
        }
    }
}

async fn send<S: AsyncRead + AsyncWrite + Unpin, P: Packet + Encode>(
    transport: &mut Transport<S>,
    packet: &P,
) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, transport.write_packet(packet))
        .await
        .map_err(|_| timed_out("limbo write timed out"))?
}

async fn send_prepared<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    packets: &PreparedPackets,
) -> io::Result<()> {
    timeout(WRITE_TIMEOUT, transport.write_prepared(packets))
        .await
        .map_err(|_| timed_out("limbo write timed out"))?
}

fn packet_id(mut frame: &[u8]) -> io::Result<i32> {
    Ok(VarInt::decode(&mut frame).map_err(invalid_data)?.0)
}
fn timed_out(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, message)
}

#[cfg(test)]
mod tests;
