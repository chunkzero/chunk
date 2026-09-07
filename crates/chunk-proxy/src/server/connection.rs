use std::{io, time::Duration};

use chunk_protocol::{
    decode_packet,
    versions::{
        SUPPORTED,
        v26_1::{Handshake, Ping, Pong, StatusRequest},
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};

use super::{
    Responses,
    authentication::{Authenticated, Authentication},
    transport::{Transport, invalid_data},
};

const INITIAL_FRAME_LIMIT: usize = 4096;

pub(super) async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    responses: &Responses,
    authentication: &Authentication,
    deadline: Duration,
    compression: Option<usize>,
    platform: Option<&super::platform::Platform>,
) -> io::Result<Option<Authenticated<S>>> {
    timeout(
        deadline,
        exchange(Transport::new(stream), responses, authentication, compression, platform),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "initial exchange timed out"))?
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    mut transport: Transport<S>,
    responses: &Responses,
    authentication: &Authentication,
    compression: Option<usize>,
    platform: Option<&super::platform::Platform>,
) -> io::Result<Option<Authenticated<S>>> {
    let handshake =
        decode_packet::<Handshake>(&transport.read_frame(INITIAL_FRAME_LIMIT).await?).map_err(invalid_data)?;
    match handshake.next_state.0 {
        1 => {
            decode_packet::<StatusRequest>(&transport.read_frame(INITIAL_FRAME_LIMIT).await?).map_err(invalid_data)?;
            if let Some(platform) = platform {
                transport
                    .write_encoded(&platform.status(handshake.server_address.as_str()).await?)
                    .await?;
            } else {
                transport.write_encoded(&responses.status).await?;
            }
            let frame = match transport.read_frame(INITIAL_FRAME_LIMIT).await {
                Ok(frame) => frame,
                Err(error) if error.kind() == io::ErrorKind::UnexpectedEof && !transport.has_buffered_data() => {
                    transport.shutdown().await?;
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            let ping = decode_packet::<Ping>(&frame).map_err(invalid_data)?;
            transport.write_packet(&Pong { payload: ping.payload }).await?;
        }
        2 if SUPPORTED
            .iter()
            .any(|version| version.protocol == handshake.protocol_version.0) =>
        {
            return authentication
                .login(transport, handshake.protocol_version.0, compression)
                .await
                .map(Some);
        }
        2 => transport.write_encoded(&responses.unsupported_version).await?,
        _ => return Err(invalid_data("unsupported handshake intention")),
    }
    transport.shutdown().await?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;
    use chunk_protocol::encode_packet;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn status_allows_clean_eof_but_rejects_truncated_ping() {
        for suffix in [&[][..], &[0x80][..]] {
            let (mut client, server) = tokio::io::duplex(4096);
            let handshake = Handshake {
                protocol_version: chunk_protocol::VarInt(775),
                server_address: chunk_protocol::McString::new("localhost").unwrap(),
                server_port: 25565,
                next_state: chunk_protocol::VarInt(1),
            };
            client.write_all(&encode_packet(&handshake).unwrap()).await.unwrap();
            client
                .write_all(&encode_packet(&StatusRequest {}).unwrap())
                .await
                .unwrap();
            client.write_all(suffix).await.unwrap();
            client.shutdown().await.unwrap();
            let responses = Responses::new(&Config::default()).unwrap();
            let result = serve(
                server,
                &responses,
                &Authentication::new().await.unwrap(),
                Duration::from_secs(10),
                None,
                None,
            )
            .await;
            if suffix.is_empty() {
                result.unwrap();
            } else {
                assert_eq!(result.err().unwrap().kind(), io::ErrorKind::UnexpectedEof);
            }
            let mut received = Vec::new();
            client.read_to_end(&mut received).await.unwrap();
            assert_eq!(received, responses.status);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn incomplete_handshake_has_a_total_deadline() {
        let (mut client, server) = tokio::io::duplex(64);
        client.write_all(&[0x80]).await.unwrap();
        let responses = Responses::new(&Config::default()).unwrap();
        let error = serve(
            server,
            &responses,
            &Authentication::new().await.unwrap(),
            Duration::from_secs(10),
            None,
            None,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    }
}
