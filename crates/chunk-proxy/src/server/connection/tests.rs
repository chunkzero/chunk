use super::*;
use crate::Config;
use chunk_protocol::encode_packet;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn status_allows_clean_eof_but_rejects_truncated_ping() {
    for suffix in [&[][..], &[0x80][..]] {
        let (mut client, server) = tokio::io::duplex(4096);
        let handshake = Handshake {
            protocol_version: chunk_protocol::VarInt(776),
            server_address: chunk_protocol::McString::new("localhost").unwrap(),
            server_port: 25565,
            next_state: chunk_protocol::VarInt(1),
        };
        client.write_all(&encode_packet(&handshake).unwrap()).await.unwrap();
        client.write_all(&encode_packet(&StatusRequest {}).unwrap()).await.unwrap();
        client.write_all(suffix).await.unwrap();
        client.shutdown().await.unwrap();
        let responses = Responses::new(&Config::default()).unwrap();
        let result =
            serve(server, &responses, &Authentication::new().await.unwrap(), Duration::from_secs(10), None, None).await;
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
    let error = serve(server, &responses, &Authentication::new().await.unwrap(), Duration::from_secs(10), None, None)
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
}
