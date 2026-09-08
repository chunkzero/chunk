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
) -> io::Result<T>
where
    S: AsyncRead + AsyncWrite + Unpin,
    I: AsyncRead + AsyncWrite + Unpin,
{
    tokio::pin!(ready);
    loop {
        tokio::select! {
            result = &mut ready => return Ok(result),
            frame = public.read_frame(INPUT_LIMIT), if receiving => {
                let frame = frame?;
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
                timeout(RPC_TIMEOUT, public.write_body(&frame)).await.map_err(io::Error::other)??;
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
