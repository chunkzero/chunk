use std::{future::Future, io};

use chunk_protocol::{
    Decode, Packet, VarInt, decode_packet,
    versions::v26_2::{
        ConfigurationAcknowledged, ConfigurationClientInformation, PlayClientInformation, StartConfiguration,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::{Instant, sleep_until, timeout},
};

use super::super::{
    configuration,
    platform::RPC_TIMEOUT,
    transport::{Transport, WRITE_TIMEOUT, invalid_data, timed_out, within},
};

const INPUT_LIMIT: usize = 65_536;
/// Output queued for one side before the relay stops reading from the other.
const BACKLOG_LIMIT: usize = 64 * 1024;

/// RPCs and destination preparation run alongside the current delivery's packet pump.
/// Frames already received are forwarded together, and each side's output is
/// written while both sides keep being read.
pub(super) async fn until<S, I, T>(
    public: &mut Transport<S>,
    internal: &mut Transport<I>,
    settings: &mut ConfigurationClientInformation,
    ready: impl Future<Output = T>,
    mut receiving: bool,
    mut commands: Option<&mut super::commands::Commands>,
) -> io::Result<T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin,
{
    tokio::pin!(ready);
    // Arrival triggers an immediate refresh; this only catches permission
    // changes for players who stay connected.
    let mut refresh = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        let read_public = receiving && internal.queued() < BACKLOG_LIMIT;
        let read_internal = public.queued() < BACKLOG_LIMIT;
        let stall = public.write_deadline().into_iter().chain(internal.write_deadline()).min();
        tokio::select! {
            result = &mut ready => {
                if let Err(error) = within(WRITE_TIMEOUT, internal.flush()).await {
                    return Err(unavailable(public, error).await);
                }
                within(WRITE_TIMEOUT, public.flush()).await?;
                return Ok(result);
            }
            _ = refresh.tick() => { if let Some(commands) = &mut commands { commands.refresh(); } }
            output = async { match &mut commands { Some(commands) => commands.receive().await, None => std::future::pending().await } } => {
                if let Some(commands) = &mut commands {
                    timeout(RPC_TIMEOUT, commands.publish(output, public)).await.map_err(io::Error::other)??;
                }
            }
            // Deadlines move as writes drain, so this only fires on the one computed for this iteration.
            () = sleep_until(stall.unwrap_or_else(Instant::now)), if stall.is_some() => {
                let now = Instant::now();
                if internal.write_deadline().is_some_and(|deadline| deadline <= now) {
                    return Err(unavailable(public, timed_out("relay write stalled")).await);
                }
                if public.write_deadline().is_some_and(|deadline| deadline <= now) {
                    return Err(timed_out("relay write stalled"));
                }
            }
            frame = public.pump(read_public.then_some(INPUT_LIMIT)), if read_public || public.queued() > 0 => {
                let mut frame = match frame {
                    // Packets sent before a player closes cleanly still reach the gameplay server.
                    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                        let _ = within(WRITE_TIMEOUT, internal.flush()).await;
                        return Err(error);
                    }
                    frame => frame?,
                };
                while let Some(body) = frame {
                    let consumed = match &commands { Some(commands) => commands.input(&body)?, None => false };
                    if !consumed {
                        retain_settings(&body, settings)?;
                        internal.queue(&body)?;
                    }
                    frame = if internal.queued() < BACKLOG_LIMIT { public.buffered_frame(INPUT_LIMIT)? } else { None };
                }
            }
            frame = internal.pump(read_internal.then_some(chunk_protocol::MAX_FRAME_SIZE)), if read_internal || internal.queued() > 0 => {
                let mut frame = frame;
                loop {
                    let body = match frame {
                        Ok(Some(body)) => body,
                        Ok(None) => break,
                        Err(error) => return Err(unavailable(public, error).await),
                    };
                    let replacement = if VarInt::decode(&mut body.as_ref()).map_err(invalid_data)?.0 == chunk_protocol::commands::CommandTree::ID {
                        commands.as_mut().map(|commands| commands.tree(&decode_packet(&body).map_err(invalid_data)?)).transpose()?
                    } else { None };
                    match replacement {
                        Some(replacement) => public.queue_encoded(&replacement)?,
                        None => public.queue(&body)?,
                    }
                    receiving = true;
                    if public.queued() >= BACKLOG_LIMIT { break; }
                    frame = internal.buffered_frame(chunk_protocol::MAX_FRAME_SIZE);
                }
            }
        }
    }
}

async fn unavailable<S: AsyncRead + AsyncWrite + Unpin>(public: &mut Transport<S>, error: io::Error) -> io::Error {
    let _ = configuration::disconnect(public, 0x20, "Gameplay server unavailable").await;
    error
}

pub(super) async fn start_configuration<S, I>(
    public: &mut Transport<S>,
    internal: &mut Transport<I>,
    settings: &mut ConfigurationClientInformation,
    commands: Option<&super::commands::Commands>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin,
{
    public.write_packet(&StartConfiguration).await?;
    loop {
        let frame = public.read_frame(INPUT_LIMIT).await?;
        if VarInt::decode(&mut frame.as_ref()).map_err(invalid_data)?.0 == ConfigurationAcknowledged::ID {
            decode_packet::<ConfigurationAcknowledged>(&frame).map_err(invalid_data)?;
            return Ok(());
        }
        if let Some(commands) = commands
            && commands.input(&frame)?
        {
            continue;
        }
        // Final source play acknowledgments can precede the configuration boundary.
        retain_settings(&frame, settings)?;
        internal.write_body(&frame).await?;
    }
}

fn retain_settings(frame: &[u8], settings: &mut ConfigurationClientInformation) -> io::Result<()> {
    if configuration::packet_id(frame)? == PlayClientInformation::ID {
        *settings = configuration::play_settings(frame)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
