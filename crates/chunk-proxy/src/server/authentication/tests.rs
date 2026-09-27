use super::*;
use chunk_protocol::versions::v26_2::ConfigurationKeepAliveResponse;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

async fn mock_session(response: String) -> (Authentication, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut auth = Authentication::new(false).await.unwrap();
    auth.endpoint = Url::parse(&format!("http://{}/hasJoined", listener.local_addr().unwrap())).unwrap();
    auth.client = Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let request = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(stream.read_u8().await.unwrap());
            assert!(request.len() < 8192);
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(request).unwrap()
    });
    (auth, request)
}

fn http_response(status: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}

fn profile_json() -> String {
    serde_json::json!({
        "id": "00112233445566778899aabbccddeeff", "name": "Alex",
        "properties": [{"name": "textures", "value": "x".repeat(1024), "signature": "signed"}]
    })
    .to_string()
}

fn encrypt_response(request: &EncryptionRequest, secret: &[u8], token: &[u8]) -> EncryptionResponse {
    let key = Rsa::public_key_from_der(request.public_key.as_slice()).unwrap();
    let encrypt = |value: &[u8]| {
        let mut bytes = vec![0; key.size() as usize];
        let count = key.public_encrypt(value, &mut bytes, Padding::PKCS1).unwrap();
        bytes.truncate(count);
        ByteArray::new(bytes).unwrap()
    };
    EncryptionResponse { shared_secret: encrypt(secret), verify_token: encrypt(token) }
}

async fn begin_login<S: AsyncRead + AsyncWrite + Unpin>(client: &mut Transport<S>) -> EncryptionRequest {
    client
        .write_packet(&LoginStart { username: McString::new("Alex").unwrap(), player_uuid: Uuid([0xff; 16]) })
        .await
        .unwrap();
    decode_packet::<EncryptionRequest>(&client.read_frame(4096).await.unwrap()).unwrap()
}

#[tokio::test]
async fn authenticated_profile_reaches_configuration_with_each_compression_mode() {
    timeout(Duration::from_secs(10), async {
        for compression in [None, Some(0), Some(256)] {
            let (auth, request_task) = mock_session(http_response("200 OK", &profile_json())).await;
            let (client, server) = tokio::io::duplex(8192);
            let mut client = Transport::new(client);
            let server = async {
                let mut accepted = auth.login(Transport::new(server), 776, compression).await.unwrap();
                assert_eq!(
                    accepted.profile.uuid,
                    Uuid([0, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff])
                );
                assert_eq!(accepted.profile.properties.as_slice()[0].signature.as_ref().unwrap().as_str(), "signed");
                // A coalesced configuration packet survives the transition together with the identity.
                let frame = accepted.transport.read_frame(4096).await.unwrap();
                assert_eq!(decode_packet::<ConfigurationKeepAliveResponse>(&frame).unwrap().keep_alive_id, 42);
            };
            let client = async {
                let request = begin_login(&mut client).await;
                assert!(request.should_authenticate);
                let secret = [0x12; 16];
                client
                    .write_packet(&encrypt_response(&request, &secret, request.verify_token.as_slice()))
                    .await
                    .unwrap();
                client.enable_encryption(&secret).unwrap();
                if let Some(threshold) = compression {
                    let packet = decode_packet::<SetCompression>(&client.read_frame(4096).await.unwrap()).unwrap();
                    assert_eq!(packet.threshold.0, i32::try_from(threshold).unwrap());
                    client.enable_compression(threshold);
                }
                let profile = decode_packet::<LoginSuccess>(&client.read_frame(65536).await.unwrap()).unwrap();
                assert_eq!(profile.username.as_str(), "Alex");
                assert_eq!(profile.properties.as_slice()[0].value.as_str(), "x".repeat(1024));
                client.write_packet(&LoginAcknowledged).await.unwrap();
                client.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: 42 }).await.unwrap();
            };
            tokio::join!(server, client);
            let request = request_task.await.unwrap();
            let query = Url::parse(&format!("http://localhost{}", request.split_whitespace().nth(1).unwrap())).unwrap();
            let params: std::collections::HashMap<_, _> = query.query_pairs().collect();
            assert_eq!(params["username"], "Alex");
            assert_eq!(params["serverId"], server_hash(&[0x12; 16], &auth.public_key));
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn offline_login_skips_encryption_and_uses_the_vanilla_offline_uuid() {
    timeout(Duration::from_secs(5), async {
        let auth = Authentication::new(true).await.unwrap();
        let (client, server) = tokio::io::duplex(8192);
        let mut client = Transport::new(client);
        let server = async {
            let accepted = auth.login(Transport::new(server), 776, Some(256)).await.unwrap();
            assert_eq!(accepted.profile.uuid.0, *uuid::uuid!("b50ad385-829d-3141-a216-7e7d7539ba7f").as_bytes());
        };
        let client = async {
            client
                .write_packet(&LoginStart { username: McString::new("Notch").unwrap(), player_uuid: Uuid([0xff; 16]) })
                .await
                .unwrap();
            // The first reply is unencrypted Set Compression rather than an Encryption Request.
            decode_packet::<SetCompression>(&client.read_frame(4096).await.unwrap()).unwrap();
            client.enable_compression(256);
            let profile = decode_packet::<LoginSuccess>(&client.read_frame(4096).await.unwrap()).unwrap();
            assert_eq!(profile.username.as_str(), "Notch");
            assert!(profile.properties.as_slice().is_empty());
            client.write_packet(&LoginAcknowledged).await.unwrap();
        };
        tokio::join!(server, client);
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn rejected_session_receives_an_encrypted_disconnect() {
    timeout(Duration::from_secs(5), async {
        let (auth, request_task) = mock_session(http_response("204 No Content", "")).await;
        let (client, server) = tokio::io::duplex(8192);
        let mut client = Transport::new(client);
        let server = async {
            assert!(auth.login(Transport::new(server), 776, Some(256)).await.is_err());
        };
        let client = async {
            let request = begin_login(&mut client).await;
            let secret = [7; 16];
            client.write_packet(&encrypt_response(&request, &secret, request.verify_token.as_slice())).await.unwrap();
            client.enable_encryption(&secret).unwrap();
            let frame = client.read_frame(4096).await.unwrap();
            let rejection = decode_packet::<LoginDisconnect>(&frame).unwrap();
            assert!(rejection.reason.as_str().contains("Unable to authenticate"));
            assert_eq!(client.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        };
        tokio::join!(server, client);
        request_task.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn session_service_fails_closed_on_bad_status_and_oversized_bodies() {
    for response in [
        http_response("503 Service Unavailable", ""),
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/\r\nContent-Length: 0\r\n\r\n".into(),
        "HTTP/1.1 200 OK\r\nContent-Length: 65537\r\n\r\n".into(),
        format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n10001\r\n{}\r\n0\r\n\r\n", " ".repeat(65537)),
        http_response("200 OK", "{}"),
    ] {
        let (auth, request) = mock_session(response).await;
        assert!(auth.verify("Alex", "hash").await.is_err());
        request.await.unwrap();
    }
}

#[tokio::test]
async fn rsa_response_requires_matching_nonce_and_exact_secret_length() {
    let auth = Authentication::new(false).await.unwrap();
    let token = [1, 2, 3, 4];
    let request = EncryptionRequest {
        server_id: McString::new("").unwrap(),
        public_key: ByteArray::new(auth.public_key.clone()).unwrap(),
        verify_token: ByteArray::new(token.to_vec()).unwrap(),
        should_authenticate: true,
    };
    for (secret, response_token) in [(&[1; 15][..], &token[..]), (&[1; 16][..], &[4, 3, 2, 1][..])] {
        assert!(auth.shared_secret(&encrypt_response(&request, secret, response_token), token).is_err());
    }
    let invalid = EncryptionResponse {
        shared_secret: ByteArray::new(vec![0; 128]).unwrap(),
        verify_token: ByteArray::new(vec![0; 128]).unwrap(),
    };
    assert!(auth.shared_secret(&invalid, token).is_err());
}

#[test]
fn signed_hash_matches_java_big_integer_vectors() {
    for (input, expected) in [
        ("Notch", "4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48"),
        ("jeb_", "-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1"),
        ("simon", "88e16a1019277b15d58faf0541e11910eb756f6"),
    ] {
        assert_eq!(signed_hex(openssl::sha::sha1(input.as_bytes())), expected);
    }
    assert_eq!(signed_hex([0; 20]), "0");
    assert_eq!(signed_hex([0xff; 20]), "-1");
}

#[test]
fn session_identity_is_validated_and_properties_preserved() {
    let valid = profile_json();
    assert!(parse_profile(valid.as_bytes(), "alex").is_ok());
    assert!(parse_profile(valid.as_bytes(), "SomeoneElse").is_err());
    assert!(
        parse_profile(
            valid.replace("00112233445566778899aabbccddeeff", "+1112233445566778899aabbccddeeff").as_bytes(),
            "Alex"
        )
        .is_err()
    );
    for name in ["", "space name", "a/b", "é", "abcdefghijklmnopq"] {
        assert!(!valid_username(name));
    }
}

#[tokio::test]
async fn total_login_deadline_includes_acknowledgment_and_closes_the_socket() {
    use super::super::{Responses, connection};
    use chunk_protocol::versions::v26_2::Handshake;

    let (auth, request_task) = mock_session(http_response("200 OK", &profile_json())).await;
    let responses = Responses::new(&crate::Config::default()).unwrap();
    let (client, server) = tokio::io::duplex(8192);
    let mut client = Transport::new(client);
    let server = async {
        let result = connection::serve(server, &responses, &auth, Duration::from_secs(10), None, None).await;
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::TimedOut);
    };
    let client = async {
        client
            .write_packet(&Handshake {
                protocol_version: VarInt(776),
                server_address: McString::new("localhost").unwrap(),
                server_port: 25565,
                next_state: VarInt(2),
            })
            .await
            .unwrap();
        let request = begin_login(&mut client).await;
        let secret = [9; 16];
        client.write_packet(&encrypt_response(&request, &secret, request.verify_token.as_slice())).await.unwrap();
        client.enable_encryption(&secret).unwrap();
        decode_packet::<LoginSuccess>(&client.read_frame(65536).await.unwrap()).unwrap();
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(11)).await;
        assert_eq!(client.read_frame(4096).await.unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    };
    tokio::join!(server, client);
    request_task.await.unwrap();
}

#[tokio::test]
async fn listener_parks_authenticated_connections_and_closes_them_on_shutdown() {
    use crate::{Config, Proxy};
    use chunk_protocol::versions::v26_2::{ConfigurationKeepAlive, Handshake};
    use std::sync::Arc;
    use tokio::{net::TcpStream, sync::oneshot};

    timeout(Duration::from_secs(5), async {
        let (auth, request_task) = mock_session(http_response("200 OK", &profile_json())).await;
        let mut proxy = Proxy::bind(
            "127.0.0.1:0".parse().unwrap(),
            Config { max_connections: std::num::NonZeroUsize::new(1).unwrap(), ..Config::default() },
        )
        .await
        .unwrap();
        proxy.authentication = Arc::new(auth);
        let address = proxy.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let server = tokio::spawn(proxy.run(async { stopped.await.map_err(io::Error::other) }));
        let mut client = Transport::new(TcpStream::connect(address).await.unwrap());
        client
            .write_packet(&Handshake {
                protocol_version: VarInt(776),
                server_address: McString::new("localhost").unwrap(),
                server_port: 25565,
                next_state: VarInt(2),
            })
            .await
            .unwrap();
        let request = begin_login(&mut client).await;
        let secret = [5; 16];
        client.write_packet(&encrypt_response(&request, &secret, request.verify_token.as_slice())).await.unwrap();
        client.enable_encryption(&secret).unwrap();
        decode_packet::<SetCompression>(&client.read_frame(4096).await.unwrap()).unwrap();
        client.enable_compression(256);
        decode_packet::<LoginSuccess>(&client.read_frame(65536).await.unwrap()).unwrap();
        client.write_packet(&LoginAcknowledged).await.unwrap();
        let keepalive = decode_packet::<ConfigurationKeepAlive>(&client.read_frame(4096).await.unwrap()).unwrap();
        client.write_packet(&ConfigurationKeepAliveResponse { keep_alive_id: keepalive.keep_alive_id }).await.unwrap();
        // A waiting player retains its capacity slot.
        let mut excess = TcpStream::connect(address).await.unwrap();
        let result = excess.read(&mut [0]).await;
        assert!(matches!(result, Ok(0)) || result.is_err_and(|error| error.kind() == io::ErrorKind::ConnectionReset));
        stop.send(()).unwrap();
        server.await.unwrap().unwrap();
        let error = client.read_frame(4096).await.unwrap_err();
        assert!(matches!(error.kind(), io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset));
        request_task.await.unwrap();
    })
    .await
    .unwrap();
}
