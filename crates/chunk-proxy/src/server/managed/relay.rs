use std::{future::Future, io};

use chunk_protocol::{
    Decode, Packet, VarInt, decode_packet,
    versions::v26_1::{
        ConfigurationAcknowledged, ConfigurationClientInformation, PlayClientInformation, StartConfiguration,
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};

use super::super::{
    configuration,
    platform::RPC_TIMEOUT,
    transport::{Transport, invalid_data},
};

const INPUT_LIMIT: usize = 65_536;

/// RPCs and destination preparation run alongside the current delivery's packet pump.
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
        tokio::select! {
            result = &mut ready => return Ok(result),
            _ = refresh.tick() => { if let Some(commands) = &mut commands { commands.refresh(); } }
            output = async { match &mut commands { Some(commands) => commands.receive().await, None => std::future::pending().await } } => {
                if let Some(commands) = &mut commands {
                    timeout(RPC_TIMEOUT, commands.publish(output, public)).await.map_err(io::Error::other)??;
                }
            }
            frame = public.read_frame(INPUT_LIMIT), if receiving => {
                let frame = frame?;
                if let Some(commands) = &commands && commands.input(&frame)? { continue; }
                retain_settings(&frame, settings)?;
                if let Err(error) = timeout(RPC_TIMEOUT, internal.write_body(&frame)).await.map_err(io::Error::other).and_then(|result| result) {
                    let _ = configuration::disconnect(public, 0x20, "Gameplay server unavailable").await;
                    return Err(error);
                }
            }
            frame = internal.read_frame(chunk_protocol::MAX_FRAME_SIZE) => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(error) => {
                        let _ = configuration::disconnect(public, 0x20, "Gameplay server unavailable").await;
                        return Err(error);
                    }
                };
                let replacement = if VarInt::decode(&mut frame.as_ref()).map_err(invalid_data)?.0 == chunk_protocol::commands::CommandTree::ID {
                    commands.as_mut().map(|commands| commands.tree(&decode_packet(&frame).map_err(invalid_data)?)).transpose()?
                } else { None };
                if let Some(replacement) = replacement {
                    timeout(RPC_TIMEOUT, public.write_encoded(&replacement)).await.map_err(io::Error::other)??;
                } else {
                    timeout(RPC_TIMEOUT, public.write_body(&frame)).await.map_err(io::Error::other)??;
                }
                receiving = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;

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

fn retain_settings(mut frame: &[u8], settings: &mut ConfigurationClientInformation) -> io::Result<()> {
    if VarInt::decode(&mut frame).map_err(invalid_data)?.0 == PlayClientInformation::ID {
        // Both protocol states use the same settings body; only their packet IDs differ.
        *settings = ConfigurationClientInformation::decode(&mut frame).map_err(invalid_data)?;
        if !frame.is_empty() {
            return Err(invalid_data("trailing client information"));
        }
    }
    Ok(())
}
