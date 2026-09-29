//! The edge between a fake management service, which streams one route, and a fake gateway.

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};

use bytes::Bytes;
use chunk_edge::{Config, Edge};
use chunk_management::v1::{Route, WatchRoutesResponse};
use http_body_util::{BodyExt, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use prost::Message;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const HANDSHAKE: &[u8] = b"\x19\x00\x88\x06\x12Play.Example.com.\0\x63\xdd\x02";
const LOGIN_START: &[u8] = b"\x06\x00\x04Alex";

/// Serves `WatchRoutes` with one route to `gateway`, and records every method called.
async fn management(gateway: SocketAddr) -> (String, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = calls.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let calls = recorded.clone();
            let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                let calls = calls.clone();
                async move {
                    assert_eq!(request.headers()["authorization"], "Bearer edge-token");
                    calls.lock().unwrap().push(request.uri().path().to_owned());
                    let routes = WatchRoutesResponse {
                        revision: 1,
                        reset: true,
                        routes: vec![Route {
                            hostname: "play.example.com".into(),
                            environment_id: "env_test".into(),
                            gateway_addresses: vec![gateway.to_string()],
                            ..Default::default()
                        }],
                        removed_hostnames: vec![],
                    };
                    let (sender, receiver) = mpsc::channel::<Result<Frame<Bytes>, Infallible>>(1);
                    sender.send(Ok(Frame::data(envelope(&routes.encode_to_vec())))).await.unwrap();
                    // The stream stays open, as management's does between changes.
                    tokio::spawn(async move { sender.closed().await });
                    let body: BoxBody<Bytes, Infallible> =
                        StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed();
                    Ok::<_, Infallible>(
                        hyper::Response::builder()
                            .header("content-type", "application/connect+proto")
                            .body(body)
                            .unwrap(),
                    )
                }
            });
            let io = hyper_util::rt::TokioIo::new(stream);
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(io, service));
        }
    });
    (format!("http://{address}"), calls)
}

fn envelope(payload: &[u8]) -> Bytes {
    let mut frame = vec![0];
    frame.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    frame.extend_from_slice(payload);
    frame.into()
}

struct Harness {
    edge: SocketAddr,
    gateway: TcpListener,
    calls: Arc<Mutex<Vec<String>>>,
    stop: CancellationToken,
}

async fn start() -> Harness {
    let gateway = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (management_url, calls) = management(gateway.local_addr().unwrap()).await;
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        management_url,
        edge_token: "edge-token".into(),
        handshake_timeout: Duration::from_millis(200),
    };
    let edge = Edge::bind(config).await.unwrap();
    let address = edge.local_addr().unwrap();
    let stop = CancellationToken::new();
    tokio::spawn(edge.run(stop.clone()));
    Harness { edge: address, gateway, calls, stop }
}

/// Connects a player that sends `HANDSHAKE` and `LOGIN_START`, again each time the edge closes it for lack of routes,
/// until the edge hands one to the gateway.
async fn connect_player(harness: &Harness) -> (TcpStream, TcpStream) {
    for _ in 0..100 {
        let mut player = TcpStream::connect(harness.edge).await.unwrap();
        player.write_all(&[HANDSHAKE, LOGIN_START].concat()).await.unwrap();
        let mut byte = [0];
        let routed = tokio::select! {
            accepted = harness.gateway.accept() => Some(accepted.unwrap().0),
            _ = player.read(&mut byte) => None,
        };
        if let Some(gateway) = routed {
            return (player, gateway);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the edge never routed the player");
}

async fn closed(stream: &mut TcpStream) {
    let read = timeout(Duration::from_secs(2), stream.read(&mut [0])).await.expect("the edge closed the connection");
    assert!(matches!(read, Ok(0) | Err(_)), "expected a closed connection, got {read:?}");
}

#[tokio::test]
async fn routes_a_player_to_the_gateway_with_its_address() {
    let harness = start().await;
    let (mut player, mut gateway) = connect_player(&harness).await;

    let mut expected = b"\r\n\r\n\0\r\nQUIT\n\x21\x11\x00\x0c\x7f\x00\x00\x01\x7f\x00\x00\x01".to_vec();
    expected.extend_from_slice(&player.local_addr().unwrap().port().to_be_bytes());
    expected.extend_from_slice(&harness.edge.port().to_be_bytes());
    expected.extend_from_slice(HANDSHAKE);
    expected.extend_from_slice(LOGIN_START);
    let mut received = vec![0; expected.len()];
    gateway.read_exact(&mut received).await.unwrap();
    assert_eq!(received, expected);

    gateway.write_all(b"from the gateway").await.unwrap();
    let mut reply = [0; 16];
    player.read_exact(&mut reply).await.unwrap();
    assert_eq!(&reply, b"from the gateway");
    player.write_all(b"from the player").await.unwrap();
    let mut sent = [0; 15];
    gateway.read_exact(&mut sent).await.unwrap();
    assert_eq!(&sent, b"from the player");

    drop(gateway);
    closed(&mut player).await;
    harness.stop.cancel();
}

#[tokio::test]
async fn closes_unknown_hostnames_and_silent_clients_without_a_wake() {
    let harness = start().await;
    drop(connect_player(&harness).await);

    let mut unknown = TcpStream::connect(harness.edge).await.unwrap();
    unknown.write_all(b"\x18\x00\x88\x06\x11other.example.com\x63\xdd\x02").await.unwrap();
    closed(&mut unknown).await;
    let mut silent = TcpStream::connect(harness.edge).await.unwrap();
    closed(&mut silent).await;

    assert!(timeout(Duration::from_millis(200), harness.gateway.accept()).await.is_err());
    assert!(harness.calls.lock().unwrap().iter().all(|path| path == "/chunk.management.v1.EdgeService/WatchRoutes"));
    harness.stop.cancel();
}
