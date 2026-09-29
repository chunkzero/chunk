//! The edge between a fake management service, whose route streams the test drives, and fake gateways.

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

const LOGIN_START: &[u8] = b"\x06\x00\x04Alex";

/// One `WatchRoutes` response in progress; dropping it ends the stream.
struct RouteStream(mpsc::Sender<Result<Frame<Bytes>, Infallible>>);

impl RouteStream {
    async fn send(&self, reset: bool, routes: Vec<Route>) {
        self.update(WatchRoutesResponse { reset, routes, ..Default::default() }).await;
    }

    async fn remove(&self, hostname: &str) {
        self.update(WatchRoutesResponse { removed_hostnames: vec![hostname.into()], ..Default::default() }).await;
    }

    async fn update(&self, update: WatchRoutesResponse) {
        let mut frame = vec![0];
        frame.extend_from_slice(&u32::try_from(update.encoded_len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&update.encode_to_vec());
        self.0.send(Ok(Frame::data(frame.into()))).await.unwrap();
    }
}

/// Serves each `WatchRoutes` call with a stream the test receives from the returned channel, and records every method
/// called.
async fn management() -> (String, mpsc::UnboundedReceiver<RouteStream>, Arc<Mutex<Vec<String>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let (streams, stream_receiver) = mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let recorded = calls.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let calls = recorded.clone();
            let streams = streams.clone();
            let service = hyper::service::service_fn(move |request: hyper::Request<Incoming>| {
                assert_eq!(request.headers()["authorization"], "Bearer edge-token");
                calls.lock().unwrap().push(request.uri().path().to_owned());
                let (sender, receiver) = mpsc::channel(1);
                _ = streams.send(RouteStream(sender));
                let body: BoxBody<Bytes, Infallible> =
                    StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed();
                let response =
                    hyper::Response::builder().header("content-type", "application/connect+proto").body(body);
                async move { Ok::<_, Infallible>(response.unwrap()) }
            });
            let io = hyper_util::rt::TokioIo::new(stream);
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(io, service));
        }
    });
    (format!("http://{address}"), stream_receiver, calls)
}

fn route(hostname: &str, gateways: &[&TcpListener]) -> Route {
    Route {
        hostname: hostname.into(),
        environment_id: "env_test".into(),
        gateway_addresses: gateways.iter().map(|gateway| gateway.local_addr().unwrap().to_string()).collect(),
        ..Default::default()
    }
}

fn handshake(address: &str) -> Vec<u8> {
    let mut packet = vec![0, 0x88, 0x06, u8::try_from(address.len()).unwrap()];
    packet.extend_from_slice(address.as_bytes());
    packet.extend_from_slice(b"\x63\xdd\x02");
    [vec![u8::try_from(packet.len()).unwrap()], packet].concat()
}

struct Harness {
    edge: SocketAddr,
    gateways: [TcpListener; 2],
    streams: mpsc::UnboundedReceiver<RouteStream>,
    calls: Arc<Mutex<Vec<String>>>,
    stop: CancellationToken,
}

impl Harness {
    async fn start() -> Self {
        let (management_url, streams, calls) = management().await;
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
        let gateways =
            [TcpListener::bind("127.0.0.1:0").await.unwrap(), TcpListener::bind("127.0.0.1:0").await.unwrap()];
        Self { edge: address, gateways, streams, calls, stop }
    }

    async fn next_stream(&mut self) -> RouteStream {
        timeout(Duration::from_secs(5), self.streams.recv()).await.expect("the edge watched routes").unwrap()
    }

    /// Connects a player that dials `address` and sends Login Start, returning it with the gateway connection the edge
    /// opened for it and that gateway's index, or None for the gateway if the edge closed the player.
    async fn dial(&self, address: &str) -> (TcpStream, Option<(usize, TcpStream)>) {
        let mut player = TcpStream::connect(self.edge).await.unwrap();
        player.write_all(&[&handshake(address)[..], LOGIN_START].concat()).await.unwrap();
        let mut byte = [0];
        let routed = timeout(Duration::from_secs(2), async {
            tokio::select! {
                accepted = self.gateways[0].accept() => Some((0, accepted.unwrap().0)),
                accepted = self.gateways[1].accept() => Some((1, accepted.unwrap().0)),
                read = player.read(&mut byte) => {
                    assert!(matches!(read, Ok(0) | Err(_)), "expected a closed connection, got {read:?}");
                    None
                }
            }
        })
        .await
        .expect("the edge routed or closed the player");
        (player, routed)
    }

    /// Where the edge sends players dialling `address`, waiting out updates the edge has yet to apply.
    async fn eventually_routes(&self, address: &str, gateway: Option<usize>) {
        for _ in 0..100 {
            if self.gateway_for(address).await == gateway {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the edge never routed {address:?} to {gateway:?}");
    }

    async fn gateway_for(&self, address: &str) -> Option<usize> {
        self.dial(address).await.1.map(|(index, _)| index)
    }
}

#[tokio::test]
async fn routes_a_player_to_the_gateway_with_its_address() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    routes.send(true, vec![route("play.example.com", &[&harness.gateways[0]])]).await;
    harness.eventually_routes("play.example.com", Some(0)).await;
    let (mut player, Some((0, mut gateway))) = harness.dial("Play.Example.com.\0").await else {
        panic!("the edge did not route the player");
    };

    let mut expected = b"\r\n\r\n\0\r\nQUIT\n\x21\x11\x00\x0c\x7f\x00\x00\x01\x7f\x00\x00\x01".to_vec();
    expected.extend_from_slice(&player.local_addr().unwrap().port().to_be_bytes());
    expected.extend_from_slice(&harness.edge.port().to_be_bytes());
    expected.extend_from_slice(&handshake("Play.Example.com.\0"));
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
    let read = timeout(Duration::from_secs(2), player.read(&mut [0])).await.expect("the edge closed the player");
    assert!(matches!(read, Ok(0) | Err(_)), "expected a closed connection, got {read:?}");
    harness.stop.cancel();
}

#[tokio::test]
async fn follows_route_changes_and_closes_unrouted_players_without_a_wake() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    let first = &harness.gateways[0];
    routes
        .send(
            true,
            vec![
                route("play.example.com", &[first]),
                route("old.example.com", &[first]),
                route("asleep.example.com", &[]),
            ],
        )
        .await;
    harness.eventually_routes("play.example.com", Some(0)).await;
    assert_eq!(harness.gateway_for("old.example.com").await, Some(0));
    assert_eq!(harness.gateway_for("asleep.example.com").await, None, "a route with no gateways");
    assert_eq!(harness.gateway_for("unknown.example.com").await, None, "an unknown hostname");
    let mut silent = TcpStream::connect(harness.edge).await.unwrap();
    let read = timeout(Duration::from_secs(2), silent.read(&mut [0])).await.expect("the edge closed a silent client");
    assert!(matches!(read, Ok(0) | Err(_)));

    routes.send(false, vec![route("play.example.com", &[&harness.gateways[1]])]).await;
    harness.eventually_routes("play.example.com", Some(1)).await;
    routes.remove("old.example.com").await;
    harness.eventually_routes("old.example.com", None).await;

    drop(routes);
    let routes = harness.next_stream().await;
    routes.send(true, vec![route("play.example.com", &[&harness.gateways[0]])]).await;
    harness.eventually_routes("play.example.com", Some(0)).await;
    assert_eq!(harness.gateway_for("old.example.com").await, None, "a hostname the reset left out");

    let calls = harness.calls.lock().unwrap().clone();
    assert_eq!(calls, ["/chunk.management.v1.EdgeService/WatchRoutes"; 2]);
    harness.stop.cancel();
}
