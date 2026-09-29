//! Which gateway process owns a `gateway/<id>` topic, and the claims a process that takes it over withdraws.

use super::{runtime::with_jvm, *};
use chunk_control::{Control, MoveRequest};
use chunk_proto::{
    control::v1::{ClaimRequest, Identity},
    sync::v1::{ActiveArguments, ClaimPhase, GatewayClaim},
};

/// Subscribes to `gateway/proxy` as process `instance`, after `after`'s stream and position when set, returning the
/// stream and its first update.
async fn subscribe(fixture: &Fixture, instance: &str, after: Option<&Update>) -> (Streaming<Update>, Update) {
    let after = after.map(|update| Cursor { stream: update.stream.clone(), position: update.position });
    let request = SubscribeRequest { after, ..gateway_topic("proxy", instance) };
    let mut client = fixture.client.clone();
    let mut updates = client.subscribe(authorized(request, &fixture.gateway)).await.unwrap().into_inner();
    let first = next(&mut updates).await;
    (updates, first)
}

fn failure(update: &Update) -> Option<Code> {
    update.error.as_ref().map(chunk_proto::sync::v1::Error::code)
}

/// Reads `updates` until the stream ends, returning why.
async fn ended(updates: &mut Streaming<Update>) -> Option<Code> {
    loop {
        let update = next(updates).await;
        if update.error.is_some() {
            return failure(&update);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_process_owns_its_topic_until_another_takes_it_over() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    let mut client = fixture.client.clone();
    let foreign = gateway_topic("other", "a");
    let mut denied = client.subscribe(authorized(foreign, &gateway)).await.unwrap().into_inner();
    assert_eq!(failure(&next(&mut denied).await), Some(Code::Denied));
    let unnamed = SubscribeRequest { topic: "gateway/proxy".into(), ..SubscribeRequest::default() };
    let mut invalid = client.subscribe(authorized(unnamed, &gateway)).await.unwrap().into_inner();
    assert_eq!(failure(&next(&mut invalid).await), Some(Code::Invalid));

    // Process A subscribes again with a stale cursor, as after a lost response, or with none, and keeps the topic.
    let (mut first, snapshot) = subscribe(&fixture, "a", None).await;
    assert!(snapshot.snapshot && !snapshot.stream.is_empty());
    let (_resumed, resumed) = subscribe(&fixture, "a", Some(&snapshot)).await;
    assert!(!resumed.snapshot && resumed.error.is_none() && resumed.stream != snapshot.stream);
    assert_eq!(ended(&mut first).await, Some(Code::Stopped));
    let (_stale, stale) = subscribe(&fixture, "a", Some(&snapshot)).await;
    assert!(!stale.snapshot && stale.error.is_none());
    let (mut current, fresh) = subscribe(&fixture, "a", None).await;
    assert!(fresh.snapshot && fresh.error.is_none());

    // Process B takes the topic over: A's stream ends, and A never subscribes again, with or without a cursor.
    let (_taken, taken) = subscribe(&fixture, "b", None).await;
    assert!(taken.error.is_none());
    assert_eq!(ended(&mut current).await, Some(Code::Superseded));
    for after in [None, Some(&fresh)] {
        assert_eq!(failure(&subscribe(&fixture, "a", after).await.1), Some(Code::Superseded));
    }
    assert_eq!(code(&fixture.call_on(&gateway, &fresh.stream).await), Code::Stopped);
    let called = fixture.call_on(&gateway, &taken.stream).await;
    assert_eq!(called.outcome, Some(Outcome::Result(b"0".to_vec())));
    let cli = fixture.cli.clone();
    assert_eq!(code(&fixture.call_on(&cli, &taken.stream).await), Code::Stopped);

    // However the owner's subscription and a new process's interleave, the new process ends up owning the topic.
    let mut owner = "b".to_owned();
    for round in 0..8 {
        let replacement = format!("c{round}");
        let (_, (_later, later)) =
            tokio::join!(subscribe(&fixture, &owner, None), subscribe(&fixture, &replacement, None));
        assert!(later.error.is_none());
        assert_eq!(failure(&subscribe(&fixture, &owner, None).await.1), Some(Code::Superseded));
        owner = replacement;
    }
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_owning_process_s_live_stream_keeps_core_awake() {
    let mut fixture = Fixture::start().await;
    let (gateway, cli) = (fixture.gateway.clone(), fixture.cli.clone());
    let liveness = |fixture: &Fixture| fixture.gateways.liveness.active();
    let active = |connections| ActiveArguments { connections };
    let (mut first, a) = subscribe(&fixture, "a", None).await;
    // A live stream core hasn't heard from yet counts as active; heard holding no connections, it doesn't.
    assert!(liveness(&fixture));
    let heard = fixture.platform(&gateway, &a.stream, "", "chunk:active", &active(0)).await;
    assert!(matches!(heard.outcome, Some(Outcome::Result(_))));
    assert!(!liveness(&fixture));

    // Process B takes the topic over, and A's reports are stopped rather than counted.
    let (second, b) = subscribe(&fixture, "b", None).await;
    assert_eq!(ended(&mut first).await, Some(Code::Superseded));
    fixture.platform(&gateway, &b.stream, "", "chunk:active", &active(0)).await;
    let refused =
        [(&gateway, a.stream.as_str(), Code::Stopped), (&gateway, "", Code::Stopped), (&cli, &b.stream, Code::Denied)];
    for (credential, stream, refusal) in refused {
        let response = fixture.platform(credential, stream, "", "chunk:active", &active(1)).await;
        assert_eq!(code(&response), refusal);
    }
    assert!(!liveness(&fixture));

    // B's connections count until its stream ends.
    fixture.platform(&gateway, &b.stream, "", "chunk:active", &active(2)).await;
    assert!(liveness(&fixture));
    drop(second);
    let ended = async {
        while liveness(&fixture) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), ended).await.expect("the ended stream stopped counting");
    fixture.stop().await;
}

/// The phase of gateway `proxy`'s claim `operation`, as control shows it.
fn phase(control: &Control, operation: &str) -> Option<ClaimPhase> {
    let (_, snapshot) = chunk_control::gateway::Topic::open(control, "proxy", None).unwrap();
    let entry = snapshot.upserts.into_iter().find(|entry| entry.key == operation)?;
    let Some(State::Value(value)) = entry.state else { panic!("a claim without a value") };
    Some(GatewayClaim::decode(&value[..]).unwrap().phase())
}

async fn reach(control: &Control, operation: &str, wanted: ClaimPhase) {
    let reached = async {
        while phase(control, operation) != Some(wanted) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), reached).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_gateway_process_withdraws_the_claims_earlier_ones_left_before_serving_and_keeps_its_own() {
    const OTHER: &str = "00000000-0000-0000-0000-000000000002";
    const STRAY: &str = "00000000-0000-0000-0000-000000000003";
    let (fixture, jvm) = with_jvm().await;
    let gateway = chunk_proxy::GatewayCredential { id: "proxy".into(), credential: fixture.gateway.clone() };
    let target = chunk_proxy::PlatformTarget { core: fixture.endpoint.clone(), gateway, deployment: "test".into() };
    let config = chunk_proxy::Config { platform: Some(target), ..chunk_proxy::Config::default() };
    let start = || async {
        let proxy = chunk_proxy::Proxy::bind("127.0.0.1:0".parse().unwrap(), config.clone()).await.unwrap();
        let (address, retarget) = (proxy.local_addr().unwrap(), proxy.retarget().unwrap());
        let stop = CancellationToken::new();
        let stopped = stop.clone();
        let running = tokio::spawn(proxy.run(async move {
            stopped.cancelled().await;
            Ok(())
        }));
        (address, retarget, stop, running)
    };
    let hold = |proxy, uuid| {
        let held = chunk_proxy::testing::hold(proxy, uuid, "player", runtime::gateway_demand("lobby"));
        async { tokio::time::timeout(Duration::from_secs(30), held).await.unwrap().unwrap() }
    };
    let control = &fixture.control;

    // Process A serves a player when process B starts under the same gateway ID, which ends A.
    let (_, earlier, _, replaced) = start().await;
    let inherited = hold(&earlier, runtime::PLAYER).await;
    jvm.stall_withdrawals();
    let (address, later, stop, running) = start().await;
    let error = tokio::time::timeout(Duration::from_secs(5), replaced).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("replaced"), "{error}");

    // B withdraws A's claim, which the JVM holds open, and serves no connection meanwhile.
    reach(control, &inherited, ClaimPhase::Withdrawing).await;
    let ping = tokio::spawn(status(address));
    let own = hold(&later, OTHER).await;
    assert!(!ping.is_finished());
    jvm.close(&inherited).await;
    let answer = tokio::time::timeout(Duration::from_secs(10), ping).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&answer).contains("description"));
    assert_eq!((phase(control, &inherited), phase(control, &own)), (None, Some(ClaimPhase::Arrived)));

    // While B serves, control moves B's player, whose destination claim keeps B's connection, and a claim of A's
    // commits only now, as one A's call made before B took over may. B withdraws A's, and would have reached `move`
    // first, since it withdraws in operation order.
    let request = MoveRequest {
        operation_id: "move".into(),
        player_id: OTHER.into(),
        demand: runtime::demand("arena"),
        ..MoveRequest::default()
    };
    control.claim(control.move_player(request).unwrap()).await.unwrap();
    let identity = Identity { uuid: STRAY.into(), username: "stray".into(), properties: vec![] };
    let stray = ClaimRequest {
        operation_id: "stray".into(),
        connection_id: "earlier/stray".into(),
        identity: Some(identity),
        ..runtime::login()
    };
    control.claim(stray).await.unwrap();
    reach(control, "stray", ClaimPhase::Withdrawing).await;
    assert_eq!(phase(control, "move"), Some(ClaimPhase::Reserved));
    let answer = tokio::time::timeout(Duration::from_secs(10), status(address)).await.unwrap();
    assert!(String::from_utf8_lossy(&answer).contains("description"));
    jvm.close("stray").await;

    stop.cancel();
    running.await.unwrap().unwrap();
    fixture.stop().await;
    jvm.abort();
}

/// Sends a status ping to `address`, returning everything the listener answers.
async fn status(address: std::net::SocketAddr) -> Vec<u8> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
    // A handshake for protocol 776 that asks for status, then the status request.
    let mut handshake = vec![0x00, 0x88, 0x06, 9];
    handshake.extend_from_slice(b"localhost");
    handshake.extend_from_slice(&address.port().to_be_bytes());
    handshake.push(0x01);
    let mut packets = vec![u8::try_from(handshake.len()).unwrap()];
    packets.extend(handshake);
    packets.extend([0x01, 0x00]);
    stream.write_all(&packets).await.unwrap();
    stream.shutdown().await.unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).await.unwrap();
    answer
}
