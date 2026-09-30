//! The edge between a fake management service, whose route streams and wake outcomes the test drives, and fake
//! gateways.

use std::{
    convert::Infallible,
    io,
    net::SocketAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use chunk_edge::{Config, Edge};
use chunk_management::v1::{
    RefundWakeRequest, Route, SleepingPingMode, WakeOutcome, WakeReason, WakeRequest, WakeResponse, WatchRoutesResponse,
};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use prost::Message;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const LOGIN_START: &[u8] = b"\x06\x00\x04Alex";
const WAKE_TIMEOUT: Duration = Duration::from_secs(1);
const LOGIN_TIMEOUT: Duration = Duration::from_millis(500);

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

/// What the fake management service saw, and how it answers `Wake`, whose refund token is always `window-1`.
struct Management {
    streams: mpsc::UnboundedSender<RouteStream>,
    /// Every method called.
    calls: Mutex<Vec<String>>,
    wakes: Mutex<Vec<WakeRequest>>,
    refunds: Mutex<Vec<RefundWakeRequest>>,
    outcome: Mutex<WakeOutcome>,
    /// How long `Wake` takes to answer.
    delay: Mutex<Duration>,
}

async fn call(
    management: Arc<Management>,
    request: hyper::Request<Incoming>,
) -> hyper::Response<BoxBody<Bytes, Infallible>> {
    assert_eq!(request.headers()["authorization"], "Bearer edge-token");
    let path = request.uri().path().to_owned();
    management.calls.lock().unwrap().push(path.clone());
    let response = hyper::Response::builder();
    if path.ends_with("/Wake") {
        let body = request.into_body().collect().await.unwrap().to_bytes();
        management.wakes.lock().unwrap().push(WakeRequest::decode(body).unwrap());
        let delay = *management.delay.lock().unwrap();
        tokio::time::sleep(delay).await;
        let outcome = *management.outcome.lock().unwrap();
        let body = WakeResponse { outcome: outcome.into(), refund_token: "window-1".into() }.encode_to_vec();
        return response.header("content-type", "application/proto").body(Full::from(body).boxed()).unwrap();
    }
    if path.ends_with("/RefundWake") {
        let body = request.into_body().collect().await.unwrap().to_bytes();
        management.refunds.lock().unwrap().push(RefundWakeRequest::decode(body).unwrap());
        return response.header("content-type", "application/proto").body(Full::from(Vec::new()).boxed()).unwrap();
    }
    let (sender, receiver) = mpsc::channel(1);
    _ = management.streams.send(RouteStream(sender));
    let body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(receiver)).boxed();
    response.header("content-type", "application/connect+proto").body(body).unwrap()
}

/// Serves `management` at the returned URL.
async fn serve(management: Arc<Management>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let management = management.clone();
            let service = hyper::service::service_fn(move |request| {
                let management = management.clone();
                async move { Ok::<_, Infallible>(call(management, request).await) }
            });
            let io = hyper_util::rt::TokioIo::new(stream);
            tokio::spawn(hyper::server::conn::http1::Builder::new().serve_connection(io, service));
        }
    });
    format!("http://{address}")
}

fn route(hostname: &str, gateways: &[&TcpListener]) -> Route {
    Route {
        hostname: hostname.into(),
        environment_id: "env_test".into(),
        gateway_addresses: gateways.iter().map(|gateway| gateway.local_addr().unwrap().to_string()).collect(),
        ..Default::default()
    }
}

fn asleep(hostname: &str, ping: SleepingPingMode, status: &Value) -> Route {
    Route { asleep: true, sleeping_ping: ping.into(), cached_status_json: status.to_string(), ..route(hostname, &[]) }
}

fn handshake(address: &str, intent: u8) -> Vec<u8> {
    let mut packet = vec![0, 0x88, 0x06, u8::try_from(address.len()).unwrap()];
    packet.extend_from_slice(address.as_bytes());
    packet.extend_from_slice(&[0x63, 0xdd, intent]);
    [vec![u8::try_from(packet.len()).unwrap()], packet].concat()
}

/// Reads one frame's packet ID and payload.
async fn frame(stream: &mut (impl AsyncRead + Unpin)) -> io::Result<Vec<u8>> {
    let mut length = 0;
    for shift in (0..21).step_by(7) {
        let byte = stream.read_u8().await?;
        length |= usize::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            break;
        }
    }
    let mut frame = vec![0; length];
    stream.read_exact(&mut frame).await?;
    Ok(frame)
}

/// The string of a packet whose only field is one, under 128 bytes long, after checking its ID.
fn string_field(packet: &[u8], id: u8) -> String {
    assert_eq!(packet[0], id);
    let length = usize::from(packet[1]);
    assert!(length < 0x80 && packet.len() == 2 + length, "a short string packet: {packet:?}");
    String::from_utf8(packet[2..].to_vec()).unwrap()
}

/// Answers each status request on `gateway` with `status`, counting them.
fn gateway_status(gateway: TcpListener, status: &Value) -> Arc<AtomicUsize> {
    let fetches = Arc::new(AtomicUsize::new(0));
    let counted = fetches.clone();
    let mut response = vec![0, u8::try_from(status.to_string().len()).unwrap()];
    response.extend_from_slice(status.to_string().as_bytes());
    let response = [vec![u8::try_from(response.len()).unwrap()], response].concat();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = gateway.accept().await {
            let mut proxy_header = [0; 28];
            stream.read_exact(&mut proxy_header).await.unwrap();
            assert_eq!(&proxy_header[..12], b"\r\n\r\n\0\r\nQUIT\n");
            assert_eq!(frame(&mut stream).await.unwrap()[0], 0, "a handshake");
            assert_eq!(frame(&mut stream).await.unwrap(), [0], "a status request");
            counted.fetch_add(1, Ordering::SeqCst);
            stream.write_all(&response).await.unwrap();
        }
    });
    fetches
}

struct Harness {
    edge: SocketAddr,
    gateways: [TcpListener; 2],
    management: Arc<Management>,
    streams: mpsc::UnboundedReceiver<RouteStream>,
    stop: CancellationToken,
}

impl Harness {
    async fn start() -> Self {
        let (streams, stream_receiver) = mpsc::unbounded_channel();
        let management = Arc::new(Management {
            streams,
            calls: Mutex::default(),
            wakes: Mutex::default(),
            refunds: Mutex::default(),
            outcome: Mutex::new(WakeOutcome::Waking),
            delay: Mutex::default(),
        });
        let config = Config {
            bind: "127.0.0.1:0".parse().unwrap(),
            management_url: serve(management.clone()).await,
            edge_token: "edge-token".into(),
            handshake_timeout: Duration::from_millis(200),
            wake_timeout: WAKE_TIMEOUT,
            login_timeout: LOGIN_TIMEOUT,
        };
        let edge = Edge::bind(config).await.unwrap();
        let address = edge.local_addr().unwrap();
        let stop = CancellationToken::new();
        tokio::spawn(edge.run(stop.clone()));
        let gateways =
            [TcpListener::bind("127.0.0.1:0").await.unwrap(), TcpListener::bind("127.0.0.1:0").await.unwrap()];
        Self { edge: address, gateways, management, streams: stream_receiver, stop }
    }

    async fn next_stream(&mut self) -> RouteStream {
        timeout(Duration::from_secs(5), self.streams.recv()).await.expect("the edge watched routes").unwrap()
    }

    fn calls(&self) -> Vec<String> {
        self.management.calls.lock().unwrap().clone()
    }

    fn wakes(&self) -> Vec<WakeRequest> {
        self.management.wakes.lock().unwrap().clone()
    }

    /// Connects a player that dials `address` and sends Login Start.
    async fn login(&self, address: &str) -> TcpStream {
        let mut player = TcpStream::connect(self.edge).await.unwrap();
        player.write_all(&[&handshake(address, 2)[..], LOGIN_START].concat()).await.unwrap();
        player
    }

    /// Logs a player in to `address`, returning it with the gateway connection the edge opened for it and that
    /// gateway's index, or None for the gateway if the edge closed the player.
    async fn dial(&self, address: &str) -> (TcpStream, Option<(usize, TcpStream)>) {
        let mut player = self.login(address).await;
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

    /// The status the edge answers for `address`, after checking it answers the ping that follows; None if it closes
    /// the connection instead.
    async fn try_ping(&self, address: &str) -> Option<Value> {
        let mut player = TcpStream::connect(self.edge).await.unwrap();
        player.write_all(&[&handshake(address, 1)[..], b"\x01\x00"].concat()).await.unwrap();
        let status = timeout(Duration::from_secs(3), frame(&mut player)).await.expect("an answer").ok()?;
        assert_eq!(status[0], 0);
        let ping = b"\x09\x01\x00\x00\x00\x00\x00\x00\x00\x2a";
        player.write_all(ping).await.unwrap();
        assert_eq!(frame(&mut player).await.unwrap(), ping[1..]);
        // Longer strings take a two-byte length.
        let json = if status[1] < 0x80 { &status[2..] } else { &status[3..] };
        Some(serde_json::from_slice(json).unwrap())
    }

    async fn ping(&self, address: &str) -> Value {
        self.try_ping(address).await.expect("a status response")
    }

    /// Waits until the edge answers `address` with a status that `matches`.
    async fn eventually_pings(&self, address: &str, matches: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..100 {
            if let Some(status) = self.try_ping(address).await
                && matches(&status)
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the edge never answered {address:?} as expected");
    }

    /// Logs a player in to `address` and returns the disconnect message the edge shows it.
    async fn turned_away(&self, address: &str) -> String {
        let mut player = self.login(address).await;
        let packet = timeout(WAKE_TIMEOUT * 3, frame(&mut player)).await.expect("a disconnect").unwrap();
        let reason: Value = serde_json::from_str(&string_field(&packet, 0)).unwrap();
        reason["text"].as_str().unwrap().to_owned()
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
    expected.extend_from_slice(&handshake("Play.Example.com.\0", 2));
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
    routes.send(true, vec![route("play.example.com", &[first]), route("old.example.com", &[first])]).await;
    harness.eventually_routes("play.example.com", Some(0)).await;
    assert_eq!(harness.gateway_for("old.example.com").await, Some(0));
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

    assert_eq!(harness.calls(), ["/chunk.management.v1.EdgeService/WatchRoutes"; 2]);
    harness.stop.cancel();
}

#[tokio::test]
async fn answers_pings_live_but_briefly_cached_while_awake_and_from_the_reported_status_while_asleep() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    let [gateway, _] = std::mem::replace(
        &mut harness.gateways,
        [TcpListener::bind("127.0.0.1:0").await.unwrap(), TcpListener::bind("127.0.0.1:0").await.unwrap()],
    );
    let live = json!({ "version": { "name": "1.21", "protocol": 776 }, "players": { "max": 20, "online": 3 } });
    routes.send(true, vec![route("play.example.com", &[&gateway])]).await;
    let fetches = gateway_status(gateway, &live);
    assert_eq!(harness.eventually_pings("play.example.com", |status| *status == live).await, live);
    assert_eq!(harness.ping("play.example.com").await, live);
    assert_eq!(fetches.load(Ordering::SeqCst), 1, "the second ping is answered from the cache");

    let reported = json!({ "players": { "max": 20, "online": 3 }, "description": "Lobby" });
    routes.send(false, vec![asleep("play.example.com", SleepingPingMode::Cache, &reported)]).await;
    let slept = json!({ "players": { "max": 20, "online": 0 }, "description": "Lobby" });
    harness.eventually_pings("play.example.com", |status| *status == slept).await;
    assert!(harness.wakes().is_empty(), "cached pings wake nothing");

    *harness.management.outcome.lock().unwrap() = WakeOutcome::Throttled;
    routes.send(false, vec![asleep("play.example.com", SleepingPingMode::Wake, &reported)]).await;
    harness.eventually_pings("play.example.com", |_| !harness.wakes().is_empty()).await;
    assert_eq!(harness.ping("play.example.com").await, slept, "a throttled wake answers from the cache");
    let wake = harness.wakes().remove(0);
    assert_eq!(wake.reason(), WakeReason::Ping);
    assert_eq!((wake.environment_id.as_str(), wake.client_address.as_str()), ("env_test", "127.0.0.1"));
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
    harness.stop.cancel();
}

#[tokio::test]
async fn wakes_a_sleeping_environment_for_a_login_and_holds_the_player_until_a_gateway_is_ready() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    routes.send(true, vec![asleep("play.example.com", SleepingPingMode::Cache, &json!({}))]).await;
    // A ping while routes load gets the edge's own answer, so wait for the reported one.
    harness
        .eventually_pings("play.example.com", |status| *status == json!({ "players": { "max": 0, "online": 0 } }))
        .await;

    let mut player = harness.login("play.example.com").await;
    for _ in 0..100 {
        if !harness.wakes().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let wake = harness.wakes().pop().expect("the login woke the environment");
    assert_eq!((wake.reason(), wake.client_address.as_str()), (WakeReason::Login, "127.0.0.1"));

    player.write_all(b"sent while held").await.unwrap();
    routes.send(false, vec![route("play.example.com", &[&harness.gateways[0]])]).await;
    let (mut gateway, _) =
        timeout(WAKE_TIMEOUT, harness.gateways[0].accept()).await.expect("routed once ready").unwrap();
    let expected = [&handshake("play.example.com", 2)[..], LOGIN_START, b"sent while held"].concat();
    let mut received = vec![0; 28 + expected.len()];
    gateway.read_exact(&mut received).await.unwrap();
    assert_eq!(received[28..], expected);
    harness.stop.cancel();
}

#[tokio::test]
async fn refunds_the_wake_of_a_woken_login_still_spliced_after_the_login_timeout() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    routes.send(true, vec![asleep("play.example.com", SleepingPingMode::Cache, &json!({}))]).await;
    harness
        .eventually_pings("play.example.com", |status| *status == json!({ "players": { "max": 0, "online": 0 } }))
        .await;

    let _players = [harness.login("play.example.com").await, harness.login("play.example.com").await];
    for _ in 0..100 {
        if harness.wakes().len() == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(harness.wakes().len(), 2, "both logins woke the environment");
    routes.send(false, vec![route("play.example.com", &[&harness.gateways[0]])]).await;
    let accept = || async { timeout(WAKE_TIMEOUT, harness.gateways[0].accept()).await.unwrap().unwrap().0 };
    let (failed, _kept) = (accept().await, accept().await);
    // The gateway closes one at its login deadline, as it does a login that never authenticates, while that player
    // stays open until the relay's grace to close runs past the login timeout.
    tokio::time::sleep(LOGIN_TIMEOUT * 4 / 5).await;
    drop(failed);
    tokio::time::sleep(LOGIN_TIMEOUT).await;
    let refunds = harness.management.refunds.lock().unwrap().clone();
    assert_eq!(refunds, [RefundWakeRequest { environment_id: "env_test".into(), refund_token: "window-1".into() }]);
    harness.stop.cancel();
}

#[tokio::test]
async fn refunds_a_wake_that_answers_after_the_environment_is_ready() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    routes.send(true, vec![asleep("play.example.com", SleepingPingMode::Cache, &json!({}))]).await;
    harness
        .eventually_pings("play.example.com", |status| *status == json!({ "players": { "max": 0, "online": 0 } }))
        .await;
    *harness.management.delay.lock().unwrap() = LOGIN_TIMEOUT;

    let _player = harness.login("play.example.com").await;
    for _ in 0..100 {
        if !harness.wakes().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    routes.send(false, vec![route("play.example.com", &[&harness.gateways[0]])]).await;
    let _gateway = timeout(WAKE_TIMEOUT / 4, harness.gateways[0].accept()).await.expect("routed before Wake answered");
    tokio::time::sleep(LOGIN_TIMEOUT * 2).await;
    assert_eq!(harness.management.refunds.lock().unwrap().len(), 1, "the late token refunded the wake");
    harness.stop.cancel();
}

#[tokio::test]
async fn routes_a_held_login_once_another_client_wakes_the_environment_despite_its_own_refusal() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    routes.send(true, vec![asleep("play.example.com", SleepingPingMode::Cache, &json!({}))]).await;
    harness
        .eventually_pings("play.example.com", |status| *status == json!({ "players": { "max": 0, "online": 0 } }))
        .await;
    *harness.management.outcome.lock().unwrap() = WakeOutcome::Blocked;
    *harness.management.delay.lock().unwrap() = WAKE_TIMEOUT / 2;

    let mut player = harness.login("play.example.com").await;
    for _ in 0..100 {
        if !harness.wakes().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(harness.wakes().len(), 1, "the login asked for a wake");
    routes.send(false, vec![route("play.example.com", &[&harness.gateways[0]])]).await;
    let mut byte = [0];
    tokio::select! {
        accepted = harness.gateways[0].accept() => drop(accepted.unwrap()),
        read = player.read(&mut byte) => panic!("the player was turned away: {read:?}"),
        () = tokio::time::sleep(WAKE_TIMEOUT / 4) => panic!("not routed before the blocked wake answered"),
    }
    harness.stop.cancel();
}

#[tokio::test]
async fn turns_away_logins_it_cannot_wake_for_with_the_reason() {
    let mut harness = Harness::start().await;
    let routes = harness.next_stream().await;
    routes.send(true, vec![asleep("play.example.com", SleepingPingMode::Cache, &json!({}))]).await;
    harness
        .eventually_pings("play.example.com", |status| *status == json!({ "players": { "max": 0, "online": 0 } }))
        .await;

    for (outcome, message) in [
        (WakeOutcome::Blocked, "This server is sleeping. Try again later."),
        (WakeOutcome::Throttled, "This server is starting. Try again in a moment."),
        (WakeOutcome::Waking, "This server is starting. Try again in a moment."),
    ] {
        *harness.management.outcome.lock().unwrap() = outcome;
        assert_eq!(harness.turned_away("play.example.com").await, message, "{outcome:?}");
    }
    assert_eq!(harness.wakes().len(), 3);
    harness.stop.cancel();
}
