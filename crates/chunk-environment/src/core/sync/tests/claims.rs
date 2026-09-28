//! A gateway's claim lifecycle over `chunk:*` calls.

use super::{runtime::with_jvm, *};
use chunk_proto::{
    sync::v1::{
        AbandonMoveArguments, ActivateResult, ClaimArguments, ClaimPhase, ClaimResult, DepartResult, GatewayClaim,
        GatewayLogin, PlayerIdentity, SessionDemand, WithdrawResult, claim_result,
    },
    v1::MovePlayerRequest,
};

impl Fixture {
    /// Follows `gateway/<id>` as `credential`, returning the stream and its first update.
    pub(super) async fn follow(&mut self, credential: &str, id: &str) -> (Streaming<Update>, Update) {
        let subscription = SubscribeRequest { topic: format!("gateway/{id}"), ..SubscribeRequest::default() };
        let mut updates = self.client.subscribe(authorized(subscription, credential)).await.unwrap().into_inner();
        let first = next(&mut updates).await;
        (updates, first)
    }

    pub(super) async fn platform(
        &mut self,
        credential: &str,
        stream: &str,
        operation: &str,
        method: &str,
        arguments: &impl Message,
    ) -> CallResponse {
        let message = CallRequest {
            operation_id: operation.into(),
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }
}

pub(super) fn login(connection: &str) -> ClaimArguments {
    ClaimArguments {
        login: Some(GatewayLogin {
            connection_id: connection.into(),
            player: Some(PlayerIdentity {
                uuid: runtime::PLAYER.into(),
                username: "player".into(),
                properties: vec![],
            }),
            demand: Some(SessionDemand {
                key: "lobby".into(),
                session_type: "bridge/default".into(),
                machine_profile: "small".into(),
            }),
            deployment: String::new(),
        }),
    }
}

/// A move of the player to an `arena` session, queued under `operation`.
fn arena(operation: &str) -> MovePlayerRequest {
    MovePlayerRequest {
        operation_id: operation.into(),
        player_id: runtime::PLAYER.into(),
        demand: Some(runtime::demand("arena")),
        ..MovePlayerRequest::default()
    }
}

/// Reads `updates` until claim `key` has arrived, returning that update and the claim.
pub(super) async fn arrival(updates: &mut Streaming<Update>, key: &str) -> (Update, GatewayClaim) {
    loop {
        let update = next(updates).await;
        let arrived = update.upserts.iter().find_map(|entry| match &entry.state {
            Some(State::Value(value)) if entry.key == key => Some(GatewayClaim::decode(value.as_slice()).unwrap())
                .filter(|claim| claim.phase() == ClaimPhase::Arrived),
            _ => None,
        });
        if let Some(claim) = arrived {
            return (update, claim);
        }
    }
}

pub(super) fn result<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_claims_activates_and_sees_its_player_arrive() {
    let (mut fixture, jvm) = with_jvm().await;
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;

    let claimed = fixture.platform(&gateway, &first.stream, "login", "chunk:claim", &login("connection")).await;
    let Some(claim_result::Outcome::Assignment(assignment)) = result::<ClaimResult>(&claimed).outcome else {
        panic!("expected an assignment");
    };
    assert_eq!(assignment.capability.len(), 32);
    let activated = fixture.platform(&gateway, &first.stream, "login", "chunk:activate", &()).await;
    assert!(!result::<ActivateResult>(&activated).waiting);

    let (_, claim) = arrival(&mut updates, "login").await;
    assert_eq!(claim.generation, assignment.generation);
    drop(updates);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_claims_a_login_with_its_gateway_credential_and_sees_it_arrive() {
    let (fixture, jvm) = with_jvm().await;
    let target = chunk_proxy::PlatformTarget {
        core: fixture.endpoint.clone(),
        gateway: chunk_proxy::GatewayCredential { id: "proxy".into(), credential: fixture.gateway.clone() },
        deployment: "test".into(),
    };
    let login = chunk_proxy::testing::login(target, runtime::PLAYER, "player", runtime::demand("lobby"));
    let session = tokio::time::timeout(Duration::from_secs(30), login).await.unwrap().unwrap();
    assert!(!session.is_empty());
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_gateway_process_replaces_a_live_one_and_withdraws_only_its_inherited_claims_before_serving() {
    const OTHER: &str = "00000000-0000-0000-0000-000000000002";
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
        let held = chunk_proxy::testing::hold(proxy, uuid, "player", runtime::demand("lobby"));
        async { tokio::time::timeout(Duration::from_secs(30), held).await.unwrap().unwrap() }
    };
    let phase = |uuid: &str| {
        let players = fixture.control.players().unwrap().players;
        let player = players.into_iter().find(|player| player.identity.as_ref().is_some_and(|id| id.uuid == uuid));
        player.map(|player| player.phase())
    };

    // Process A serves a player when process B starts under the same gateway ID, which ends A.
    let (_, earlier, _, replaced) = start().await;
    let inherited = hold(&earlier, runtime::PLAYER).await;
    jvm.stall_withdrawals();
    let (address, later, stop, running) = start().await;
    let error = tokio::time::timeout(Duration::from_secs(5), replaced).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("replaced"), "{error}");

    // B withdraws A's claim, which the JVM holds open, and serves no connection meanwhile.
    while phase(runtime::PLAYER) != Some(chunk_proto::v1::ClaimPhase::Withdrawing) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let ping = tokio::spawn(status(address));
    // A claim B takes after its first view of the topic isn't one it inherited.
    hold(&later, OTHER).await;
    assert!(!ping.is_finished());
    jvm.close(&inherited).await;
    let answer = tokio::time::timeout(Duration::from_secs(10), ping).await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&answer).contains("description"));
    assert_eq!((phase(runtime::PLAYER), phase(OTHER)), (None, Some(chunk_proto::v1::ClaimPhase::Arrived)));

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn claim_calls_naming_a_superseded_or_foreign_stream_are_stopped() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    let other = fixture.gateways.mint("other");
    let (older, superseded) = fixture.follow(&gateway, "proxy").await;
    let (newer, current) = fixture.follow(&gateway, "proxy").await;

    let stale = [(&gateway, superseded.stream.as_str()), (&gateway, ""), (&other, current.stream.as_str())];
    for (credential, stream) in stale {
        let response = fixture.platform(credential, stream, "login", "chunk:withdraw", &()).await;
        assert_eq!(code(&response), Code::Stopped);
    }
    let withdrawn = fixture.platform(&gateway, &current.stream, "login", "chunk:withdraw", &()).await;
    assert!(result::<WithdrawResult>(&withdrawn).unknown);
    drop((older, newer));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_claim_replays_its_outcome_and_rejects_a_changed_request_or_another_gateway() {
    let (mut fixture, jvm) = with_jvm().await;
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;
    let stream = first.stream.as_str();

    let claimed = fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    let replayed = fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    assert_eq!(result::<ClaimResult>(&replayed), result::<ClaimResult>(&claimed));
    let changed = fixture.platform(&gateway, stream, "login", "chunk:claim", &login("elsewhere")).await;
    assert_eq!(code(&changed), Code::OperationMismatch);
    let as_move = fixture.platform(&gateway, stream, "login", "chunk:claim", &ClaimArguments::default()).await;
    assert_eq!(code(&as_move), Code::OperationMismatch);

    // A login under a move's operation ID mismatches whether the move is queued or reserved.
    fixture.platform(&gateway, stream, "login", "chunk:activate", &()).await;
    arrival(&mut updates, "login").await;
    fixture.control.move_player(arena("move")).unwrap();
    let queued = fixture.platform(&gateway, stream, "move", "chunk:claim", &login("connection")).await;
    assert_eq!(code(&queued), Code::OperationMismatch);
    let moved = fixture.platform(&gateway, stream, "move", "chunk:claim", &ClaimArguments::default()).await;
    assert!(matches!(result::<ClaimResult>(&moved).outcome, Some(claim_result::Outcome::Assignment(_))));
    let reserved = fixture.platform(&gateway, stream, "move", "chunk:claim", &login("connection")).await;
    assert_eq!(code(&reserved), Code::OperationMismatch);

    let other = fixture.gateways.mint("other");
    let (foreign, stream) = fixture.follow(&other, "other").await;
    let withdrawn = fixture.platform(&other, &stream.stream, "login", "chunk:withdraw", &()).await;
    assert_eq!(code(&withdrawn), Code::Denied);
    drop((updates, foreign));
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_abandons_a_move_then_withdraws_its_arrived_claim_and_departs() {
    let (mut fixture, jvm) = with_jvm().await;
    let gateway = fixture.gateway.clone();
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;
    let stream = first.stream.as_str();
    fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    fixture.platform(&gateway, stream, "login", "chunk:activate", &()).await;
    arrival(&mut updates, "login").await;

    fixture.control.move_player(arena("move")).unwrap();
    let reason = AbandonMoveArguments { reason: "the destination refused the player".into() };
    let abandoned = fixture.platform(&gateway, stream, "move", "chunk:abandon_move", &reason).await;
    assert!(!result::<WithdrawResult>(&abandoned).unknown);

    let withdrawn = fixture.platform(&gateway, stream, "login", "chunk:withdraw", &()).await;
    assert!(!result::<WithdrawResult>(&withdrawn).unknown);
    while !next(&mut updates).await.removed.iter().any(|key| key == "login") {}
    let departed = fixture.platform(&gateway, stream, "login", "chunk:depart", &()).await;
    assert!(result::<DepartResult>(&departed).departed);
    drop(updates);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_gateways_call_claim_methods() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    for credential in [cli.as_str(), JVM] {
        let response = fixture.platform(credential, "", "login", "chunk:claim", &login("connection")).await;
        assert_eq!(code(&response), Code::Denied);
    }
    let unknown = fixture.platform(&cli, "", "login", "chunk:unknown", &()).await;
    assert_eq!(code(&unknown), Code::Invalid);
    fixture.stop().await;
}
