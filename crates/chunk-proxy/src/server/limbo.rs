mod packets;
mod world;

pub(super) use packets::Cache;
use packets::Packets;

use std::{future::Future, io, time::Duration};

use chunk_protocol::{
    Encode, Packet, decode_packet,
    versions::v26_2::{
        ChunkBatchReceived, ConfigurationClientInformation, ConfirmTeleport, PlayClientInformation, PlayKeepAlive,
        PlayKeepAliveResponse, PlayPing, PlayPong, PlayerLoaded,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until, timeout},
};

use super::{
    authentication::Authenticated,
    configuration::{self, FRAME_LIMIT, KeepAlive, packet_id},
    transport::{PreparedPackets, Transport, WRITE_TIMEOUT, invalid_data, timed_out, within},
};

const LIMBO_TIMEOUT: Duration = Duration::from_secs(60);

const ACK_TIMEOUT: Duration = Duration::from_secs(15);

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
            configuration::finish(
                &mut authenticated.transport,
                &mut settings,
                &packets.known_packs,
                &packets.configuration,
            ),
        )
        .await
        .map_err(|_| timed_out("limbo configuration timed out"))??;
        send_prepared(&mut authenticated.transport, &packets.spawn).await?;
        tracing::info!(
            username = authenticated.profile.username.as_str(),
            "limbo spawn sent; awaiting client acknowledgments"
        );
        play(authenticated, settings, destination, packets).await
    })
    .await
    .map_err(|_| timed_out("limbo waiting limit reached"))?
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
    let mut keep_alive = KeepAlive::new();
    let mut teleport = Some(Instant::now() + ACK_TIMEOUT);
    let loading_deadline = Instant::now() + ACK_TIMEOUT;
    let mut batch_received = false;
    let mut loaded = false;
    loop {
        if keep_alive.idle()
            && teleport.is_none()
            && batch_received
            && loaded
            && let Some(destination) = ready.take()
        {
            return Ok((authenticated, settings, destination));
        }
        tokio::select! {
            biased;
            () = sleep_until(teleport.unwrap_or(loading_deadline)), if teleport.is_some() => return Err(timed_out("teleport acknowledgment timed out")),
            () = sleep_until(loading_deadline), if !batch_received || !loaded => return Err(timed_out("limbo loading timed out")),
            result = &mut destination, if ready.is_none() => ready = Some(result?),
            () = sleep_until(keep_alive.deadline()) => {
                let id = keep_alive.start().ok_or_else(|| timed_out("play keepalive timed out"))?;
                send(&mut authenticated.transport, &PlayKeepAlive { keep_alive_id: id }).await?;
            }
            frame = authenticated.transport.read_frame(FRAME_LIMIT) => {
                let frame = frame?;
                match packet_id(&frame)? {
                    PlayKeepAliveResponse::ID => {
                        let id = decode_packet::<PlayKeepAliveResponse>(&frame).map_err(invalid_data)?.keep_alive_id;
                        if !keep_alive.acknowledge(id) { return Err(invalid_data("unexpected play keepalive")); }
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
                    PlayClientInformation::ID => {
                        settings = configuration::play_settings(&frame)?;
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
    within(WRITE_TIMEOUT, transport.write_packet(packet)).await
}

async fn send_prepared<S: AsyncRead + AsyncWrite + Unpin>(
    transport: &mut Transport<S>,
    packets: &PreparedPackets,
) -> io::Result<()> {
    within(WRITE_TIMEOUT, transport.write_prepared(packets)).await
}

#[cfg(test)]
mod tests;
