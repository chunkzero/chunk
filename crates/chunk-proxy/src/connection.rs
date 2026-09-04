use std::{io, time::Duration};

use bytes::{Bytes, BytesMut};
use chunk_protocol::{
    decode_frame, decode_packet, encode_packet,
    versions::SUPPORTED,
    versions::v26_1::{Handshake, Ping, Pong, StatusRequest},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

use super::Responses;

// Bounds handshake and status frames.
const INITIAL_FRAME_LIMIT: usize = 4096;

pub(super) async fn serve(
    stream: impl AsyncRead + AsyncWrite + Unpin,
    responses: &Responses,
    deadline: Duration,
) -> io::Result<()> {
    timeout(deadline, exchange(stream, responses))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "initial exchange timed out"))?
}

async fn exchange(mut stream: impl AsyncRead + AsyncWrite + Unpin, responses: &Responses) -> io::Result<()> {
    let mut buffer = BytesMut::new();
    let frame = read_frame(&mut stream, &mut buffer).await?;
    let handshake = decode_packet::<Handshake>(&frame).map_err(invalid_packet)?;
    match handshake.next_state.0 {
        1 => {
            let frame = read_frame(&mut stream, &mut buffer).await?;
            decode_packet::<StatusRequest>(&frame).map_err(invalid_packet)?;
            stream.write_all(&responses.status).await?;
            let frame = read_frame(&mut stream, &mut buffer).await?;
            let ping = decode_packet::<Ping>(&frame).map_err(invalid_packet)?;
            let response = encode_packet(&Pong { payload: ping.payload }).map_err(invalid_packet)?;
            stream.write_all(&response).await?;
        }
        // Reject before Login Start without accepting a player identity.
        2 => {
            let response = if SUPPORTED
                .iter()
                .any(|version| version.protocol == handshake.protocol_version.0)
            {
                &responses.disconnect
            } else {
                &responses.unsupported_version
            };
            stream.write_all(response).await?;
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported handshake intention",
            ));
        }
    }
    stream.shutdown().await
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin), buffer: &mut BytesMut) -> io::Result<Bytes> {
    loop {
        if let Some(frame) = decode_frame(buffer, INITIAL_FRAME_LIMIT).map_err(invalid_packet)? {
            return Ok(frame);
        }
        let mut bytes = [0; 1024];
        let count = stream.read(&mut bytes).await?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "client closed the connection",
            ));
        }
        buffer.extend_from_slice(&bytes[..count]);
    }
}

fn invalid_packet(error: chunk_protocol::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[tokio::test(start_paused = true)]
    async fn incomplete_handshake_has_a_total_deadline() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(&[0x80]).await.unwrap();
        let responses = Responses::new(&Config::default()).unwrap();
        let error = serve(server, &responses, Duration::from_secs(10)).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    }
}
