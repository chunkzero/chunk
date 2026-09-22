#![cfg(feature = "mc-26-2")]

use std::{io, time::Duration};

use bytes::BytesMut;
use chunk_protocol::{
    decode_frame, decode_packet,
    versions::v26_2::{EncryptionRequest, LoginDisconnect, StatusResponse},
};
use chunk_proxy::{Config, Proxy};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::oneshot,
    time::timeout,
};

async fn read_frame(stream: &mut TcpStream) -> Vec<u8> {
    timeout(Duration::from_secs(2), async {
        let mut buffer = BytesMut::new();
        loop {
            if let Some(frame) = decode_frame(&mut buffer, 32767).unwrap() {
                return frame.to_vec();
            }
            // Read one byte so this helper cannot discard a coalesced next frame.
            buffer.extend_from_slice(&[stream.read_u8().await.unwrap()]);
        }
    })
    .await
    .unwrap()
}

async fn closed(stream: &mut TcpStream) {
    let result = timeout(Duration::from_secs(2), stream.read(&mut [0])).await.unwrap();
    match result {
        Ok(0) => {}
        Err(error) if error.kind() == io::ErrorKind::ConnectionReset => {}
        result => panic!("expected closed socket, got {result:?}"),
    }
}

#[tokio::test]
async fn status_handles_fragmentation_and_multiple_protocol_versions() {
    let proxy = Proxy::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config { motd: "hello \"player\"\nwelcome".into(), ..Config::default() },
    )
    .await
    .unwrap();
    let address = proxy.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(proxy.run(async {
        stopped.await.unwrap();
        Ok(())
    }));
    for version in [&[47][..], &[0x88, 6]] {
        // 1.8 and 26.2 can query status; only 26.2 is enabled for login.
        let mut client = TcpStream::connect(address).await.unwrap();
        let mut handshake = vec![u8::try_from(14 + version.len()).unwrap(), 0];
        handshake.extend_from_slice(version);
        handshake.extend_from_slice(b"\x09localhost\x63\xdd\x01");
        for byte in handshake {
            client.write_all(&[byte]).await.unwrap();
        }
        // A coalesced status request and ping, with a signed 64-bit payload.
        client.write_all(&[1, 0, 9, 1, 0x80, 0, 0, 0, 0, 0, 0, 1]).await.unwrap();
        let frame = read_frame(&mut client).await;
        let status = decode_packet::<StatusResponse>(&frame).unwrap();
        let json: serde_json::Value = serde_json::from_str(status.json.as_str()).unwrap();
        assert_eq!(json["description"]["text"], "hello \"player\"\nwelcome");
        assert_eq!(json["version"]["protocol"], 776);
        assert_eq!(json["version"]["name"], "26.2");
        assert_eq!(read_frame(&mut client).await, [1, 0x80, 0, 0, 0, 0, 0, 0, 1]);
        closed(&mut client).await;
    }
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn challenges_login_and_rejects_malformed_clients_and_closes_idle_sockets_on_shutdown() {
    let proxy = Proxy::bind(
        "127.0.0.1:0".parse().unwrap(),
        Config { max_connections: std::num::NonZeroUsize::new(1).unwrap(), ..Config::default() },
    )
    .await
    .unwrap();
    let address = proxy.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(proxy.run(async {
        stopped.await.unwrap();
        Ok(())
    }));
    let mut malformed = TcpStream::connect(address).await.unwrap();
    malformed.write_all(&[0x80, 0x80, 0x80]).await.unwrap();
    closed(&mut malformed).await;
    let mut login = TcpStream::connect(address).await.unwrap();
    login.write_all(b"\x10\x00\x88\x06\x09localhost\x63\xdd\x02").await.unwrap();
    // Login Start is pipelined after the handshake. Claimed UUID is not an identity.
    login.write_all(b"\x16\x00\x04Alex\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00").await.unwrap();
    let frame = read_frame(&mut login).await;
    let request = decode_packet::<EncryptionRequest>(&frame).unwrap();
    assert!(request.should_authenticate);
    assert_eq!(request.verify_token.as_slice().len(), 4);
    login.shutdown().await.unwrap();
    closed(&mut login).await;
    let mut outdated = TcpStream::connect(address).await.unwrap();
    outdated.write_all(b"\x0f\x00\x2f\x09localhost\x63\xdd\x02").await.unwrap();
    let frame = read_frame(&mut outdated).await;
    let disconnect = decode_packet::<LoginDisconnect>(&frame).unwrap();
    let json: serde_json::Value = serde_json::from_str(disconnect.reason.as_str()).unwrap();
    assert_eq!(json["text"], "Unsupported Minecraft version. This edge supports 26.2.");
    closed(&mut outdated).await;
    let mut idle = TcpStream::connect(address).await.unwrap();
    // Status response ensures this socket has an active task waiting for ping.
    idle.write_all(b"\x0f\x00\x2f\x09localhost\x63\xdd\x01\x01\x00").await.unwrap();
    read_frame(&mut idle).await;
    let mut excess = TcpStream::connect(address).await.unwrap();
    closed(&mut excess).await;
    stop.send(()).unwrap();
    timeout(Duration::from_secs(2), server).await.unwrap().unwrap().unwrap();
    closed(&mut idle).await;
}
