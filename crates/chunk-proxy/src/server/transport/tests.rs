use super::*;
use chunk_protocol::{
    decode_packet,
    versions::v26_2::{ConfigurationKeepAliveResponse, LoginAcknowledged},
};

#[tokio::test]
async fn prepared_packets_are_reusable_across_connections_and_compression_modes() {
    use chunk_protocol::{McString, RemainingBytes, versions::v26_2::ConfigurationPluginMessage};

    let large = ConfigurationPluginMessage {
        channel: McString::new("minecraft:brand").unwrap(),
        data: RemainingBytes::new(vec![42; 1024]).unwrap(),
    };
    for compression in [None, Some(0), Some(256)] {
        let mut prepared = PreparedPackets::new(compression);
        prepared.push(&LoginAcknowledged).unwrap();
        prepared.push(&large).unwrap();
        for secret in [[1; 16], [2; 16]] {
            let (client, server) = tokio::io::duplex(4096);
            let mut client = Transport::new(client);
            let mut server = Transport::new(server);
            for transport in [&mut client, &mut server] {
                transport.enable_encryption(&secret).unwrap();
                if let Some(threshold) = compression {
                    transport.enable_compression(threshold);
                }
            }
            for _ in 0..2 {
                server.write_prepared(&prepared).await.unwrap();
                server.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: 123 }).await.unwrap();
                decode_packet::<LoginAcknowledged>(&client.read_frame(4096).await.unwrap()).unwrap();
                assert_eq!(
                    decode_packet::<ConfigurationPluginMessage>(&client.read_frame(4096).await.unwrap()).unwrap(),
                    large
                );
                assert_eq!(
                    decode_packet::<ConfigurationKeepAliveResponse>(&client.read_frame(4096).await.unwrap())
                        .unwrap()
                        .keep_alive_id,
                    123
                );
            }
            let mismatch = PreparedPackets::new(Some(17));
            assert_eq!(server.write_prepared(&mismatch).await.unwrap_err().kind(), io::ErrorKind::InvalidData);
        }
    }
}

#[test]
fn compressed_frames_enforce_threshold_and_exact_size() {
    let body = [1; 4096];
    let mut framed = Vec::new();
    encode_frame(&mut framed, &body, Some(256)).unwrap();
    let mut buffer = BytesMut::from(framed.as_slice());
    let frame = decode_frame(&mut buffer, MAX_FRAME_SIZE).unwrap().unwrap();
    assert_eq!(&inflate(&frame, 256, 4096).unwrap()[..], &body);
    assert!(inflate(&frame, 256, 4095).is_err());
    assert!(inflate(&frame.slice(..frame.len() - 1), 256, 4096).is_err());
    assert!(inflate(&Bytes::from_static(&[0, 1, 2]), 2, 4096).is_err());
    assert!(inflate(&Bytes::from_static(&[0]), 256, 4096).is_err());
    assert!(inflate(&Bytes::from_static(&[0xff, 0xff, 0xff, 0xff, 0x0f]), 256, 4096).is_err());
    let mut smaller = frame.to_vec();
    smaller[0] = 0xff;
    smaller[1] = 0x1f;
    assert!(inflate(&smaller.into(), 256, 4096).is_err());
}

#[tokio::test]
async fn encryption_switch_decrypts_buffered_bytes_and_keeps_cipher_state_between_reads() {
    let (mut client, server) = tokio::io::duplex(4096);
    let secret = [0x12; 16];
    let mut cipher = cfb8(&secret, true).unwrap();
    let acknowledgment = encode_packet(&LoginAcknowledged).unwrap();
    let mut encrypted = acknowledgment.clone();
    apply(&mut cipher, &mut encrypted).unwrap();
    let mut wire = acknowledgment;
    wire.extend(encrypted);
    client.write_all(&wire).await.unwrap();
    let mut server = Transport::new(server);
    server.read_frame(4096).await.unwrap();
    assert!(server.has_buffered_data());
    server.enable_encryption(&secret).unwrap();
    decode_packet::<LoginAcknowledged>(&server.read_frame(4096).await.unwrap()).unwrap();
    let mut encrypted = encode_packet(&ConfigurationKeepAliveResponse { keep_alive_id: 42 }).unwrap();
    apply(&mut cipher, &mut encrypted).unwrap();
    let sender = async {
        for byte in encrypted {
            client.write_all(&[byte]).await.unwrap();
            tokio::task::yield_now().await;
        }
    };
    let receiver = async {
        assert_eq!(
            decode_packet::<ConfigurationKeepAliveResponse>(&server.read_frame(4096).await.unwrap())
                .unwrap()
                .keep_alive_id,
            42
        );
    };
    tokio::join!(sender, receiver);
}
