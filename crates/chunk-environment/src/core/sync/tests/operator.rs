//! The operator's topics and methods over the sync protocol.

use super::{
    claims::{arrival, login},
    runtime::with_jvm,
    *,
};
use chunk_proto::{
    sync::v1::{
        ActivateResult, ClaimArguments, ClaimPhase, ClaimResult, DrainArguments, DrainResult, GatewayClaim, JvmHealth,
        MovePlayerArguments, MovePlayerResult, Node, NodePhase, OperatorPlayer, SessionDemand, WithdrawResult,
        claim_result, drain_arguments::Target,
    },
    v1::{AbandonMoveRequest, ActivateClaim, ClaimPhase as ControlPhase},
};

impl Fixture {
    async fn watch(&mut self, credential: &str, topic: &str, after: Option<Cursor>) -> Streaming<Update> {
        let subscription = SubscribeRequest { topic: topic.into(), after, ..SubscribeRequest::default() };
        self.client.subscribe(authorized(subscription, credential)).await.unwrap().into_inner()
    }

    async fn operate(
        &mut self,
        credential: &str,
        operation: &str,
        method: &str,
        arguments: &impl Message,
    ) -> CallResponse {
        let message = CallRequest {
            operation_id: operation.into(),
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            ..CallRequest::default()
        };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }

    /// Admits the fake player, returning the host serving them once they arrived.
    async fn arrive(&self) -> String {
        let control = self.control.clone();
        let assignment = control.claim(runtime::login()).await.unwrap();
        control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
        loop {
            let players = control.players().unwrap().players;
            if let Some(player) = players.iter().find(|player| player.phase() == ControlPhase::Arrived) {
                return player.host_id.clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

fn value<T: Message + Default>(update: &Update, key: &str) -> Option<T> {
    update.upserts.iter().find_map(|entry| match &entry.state {
        Some(State::Value(value)) if entry.key == key => Some(T::decode(value.as_slice()).unwrap()),
        _ => None,
    })
}

/// Reads `updates` until one sets `key` to a value `matching`, returning it with that value.
async fn until<T: Message + Default>(
    updates: &mut Streaming<Update>,
    key: &str,
    matching: impl Fn(&T) -> bool,
) -> (Update, T) {
    loop {
        let update = next(updates).await;
        assert!(update.error.is_none(), "{:?}", update.error);
        if let Some(found) = value::<T>(&update, key).filter(&matching) {
            return (update, found);
        }
    }
}

fn result<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

fn drain(target: Target, timeout_seconds: u32) -> DrainArguments {
    DrainArguments { target: Some(target), timeout_seconds }
}

/// A move of the fake player to a `key` session.
fn move_to(key: &str) -> MovePlayerArguments {
    let destination =
        SessionDemand { key: key.into(), session_type: "bridge/default".into(), machine_profile: "small".into() };
    MovePlayerArguments { player: runtime::PLAYER.into(), destination: Some(destination) }
}

/// The cursor after `latest`, on the stream `first` named.
fn cursor(first: &Update, latest: &Update) -> Cursor {
    Cursor { stream: first.stream.clone(), position: latest.position }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_follow_health_and_a_host_drain_until_it_stopped() {
    let (mut fixture, jvm) = with_jvm().await;
    let host = fixture.arrive().await;
    let cli = fixture.cli.clone();
    let mut nodes = fixture.watch(&cli, "nodes", None).await;
    let snapshot = next(&mut nodes).await;
    assert!(snapshot.snapshot && !snapshot.stream.is_empty());
    let node: Node = value(&snapshot, &host).expect("the host's node");
    assert_eq!(
        (node.deployment.as_str(), node.app.as_str(), node.machine_profile.as_str()),
        ("test", "bridge", "small")
    );

    // Health isn't in the log, so the next health pass sends it at an unchanged position.
    jvm.health(JvmHealth { ready: true, tick_count: 100, ..JvmHealth::default() }).await;
    let healthy = |node: &Node| node.health.as_ref().is_some_and(|health| health.tick_count == 100);
    let (update, node) = until(&mut nodes, &host, healthy).await;
    assert_eq!(node.phase(), NodePhase::Online);
    assert_eq!(update.position, snapshot.position);
    // A subscription starts from a snapshot even when it names a cursor.
    let mut resumed = fixture.watch(&cli, "nodes", Some(cursor(&snapshot, &update))).await;
    let resumed = next(&mut resumed).await;
    assert!(resumed.snapshot && value::<Node>(&resumed, &host).is_some_and(|node| healthy(&node)));

    let host_drain = drain(Target::Host(host.clone()), 1);
    let drained = fixture.operate(&cli, "operator:drain-host", "chunk:drain", &host_drain).await;
    let drained: DrainResult = result(&drained);
    assert_eq!(drained.host, host);
    let (_, node) = until(&mut nodes, &host, |node: &Node| node.phase() == NodePhase::Draining).await;
    assert_eq!((node.drain_deadline_ms, node.remaining_claims), (drained.deadline_ms, 1));
    until(&mut nodes, &host, |node: &Node| node.phase() == NodePhase::Stopping).await;
    until(&mut nodes, &host, |node: &Node| node.phase() == NodePhase::Stopped).await;
    drop(nodes);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn players_follow_claims_and_moves_under_operation_ids_bound_to_the_move() {
    let (mut fixture, jvm) = with_jvm().await;
    let (cli, control) = (fixture.cli.clone(), fixture.control.clone());
    let mut players = fixture.watch(&cli, "players", None).await;
    let snapshot = next(&mut players).await;
    assert!(snapshot.snapshot && snapshot.upserts.is_empty());

    let host = fixture.arrive().await;
    let arrived = |player: &OperatorPlayer| player.phase() == ClaimPhase::Arrived;
    let (_, player) = until(&mut players, runtime::PLAYER, arrived).await;
    assert_eq!(
        (player.username.as_str(), player.host.as_str(), player.app.as_str()),
        ("player", host.as_str(), "bridge")
    );
    assert_eq!(player.demand.map(|demand| demand.key).as_deref(), Some("lobby"));
    assert!(!player.moving);

    let arena = move_to("arena");
    let moved = fixture.operate(&cli, "operator:move", "chunk:move_player", &arena).await;
    assert_eq!(result::<MovePlayerResult>(&moved), MovePlayerResult {});
    assert!(moved.position.is_some());
    until(&mut players, runtime::PLAYER, |player: &OperatorPlayer| player.moving).await;

    assert_eq!(fixture.operate(&cli, "operator:move", "chunk:move_player", &arena).await.outcome, moved.outcome);
    let elsewhere = fixture.operate(&cli, "operator:move", "chunk:move_player", &move_to("elsewhere")).await;
    assert_eq!(code(&elsewhere), Code::OperationMismatch);
    let player_drain = drain(Target::Player(runtime::PLAYER.into()), 10);
    let drained = fixture.operate(&cli, "operator:move", "chunk:drain", &player_drain).await;
    assert_eq!(code(&drained), Code::OperationMismatch);
    for operation in ["login", "prep:1"] {
        assert_eq!(code(&fixture.operate(&cli, operation, "chunk:move_player", &arena).await), Code::Invalid);
    }

    // The gateway claims the destination, releases the source, then activates the destination.
    let destination = control.stored_claim("operator:move").unwrap().expect("the queued move").request;
    let assignment = control.claim(destination).await.unwrap();
    control.cancel(runtime::login()).await.unwrap();
    control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
    let in_arena = |player: &OperatorPlayer| {
        player.phase() == ClaimPhase::Arrived && player.demand.as_ref().is_some_and(|demand| demand.key == "arena")
    };
    let (_, player) = until(&mut players, runtime::PLAYER, in_arena).await;
    assert!(!player.moving && player.last_move_failure.is_none());

    let back = fixture.operate(&cli, "operator:back", "chunk:move_player", &move_to("lobby")).await;
    assert_eq!(result::<MovePlayerResult>(&back), MovePlayerResult {});
    let queued = control.stored_claim("operator:back").unwrap().expect("the queued move").request;
    let reason = "the lobby refused the player";
    control.abandon_move(AbandonMoveRequest { claim: Some(queued), reason: reason.into() }).await.unwrap();
    let failed = |player: &OperatorPlayer| player.last_move_failure.is_some();
    let (latest, player) = until(&mut players, runtime::PLAYER, failed).await;
    let failure = player.last_move_failure.clone().expect("the failed move");
    assert_eq!((failure.reason.as_str(), failure.destination.map(|demand| demand.key)), (reason, Some("lobby".into())));
    assert!(!player.moving && in_arena(&player));

    // A subscription starts from a snapshot even when it names a cursor.
    let mut resumed = fixture.watch(&cli, "players", Some(cursor(&snapshot, &latest))).await;
    let resumed = next(&mut resumed).await;
    assert!(resumed.snapshot);
    assert_eq!(value::<OperatorPlayer>(&resumed, runtime::PLAYER), Some(player));

    // The player disconnects, and the gateway withdraws their claim.
    let current = control.stored_claim("operator:move").unwrap().expect("the arrived claim").request;
    control.cancel(current).await.unwrap();
    while !next(&mut players).await.removed.iter().any(|key| key == runtime::PLAYER) {}
    drop(players);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_carries_out_an_operator_move_but_logs_in_under_no_operator_id() {
    let (mut fixture, jvm) = with_jvm().await;
    let (gateway, cli) = (fixture.gateway.clone(), fixture.cli.clone());
    let (mut updates, first) = fixture.follow(&gateway, "proxy").await;
    let stream = first.stream.as_str();
    let reserved = fixture.platform(&gateway, stream, "operator:login", "chunk:claim", &login("connection")).await;
    assert_eq!(code(&reserved), Code::Invalid);
    fixture.platform(&gateway, stream, "login", "chunk:claim", &login("connection")).await;
    fixture.platform(&gateway, stream, "login", "chunk:activate", &()).await;
    arrival(&mut updates, "login").await;

    // The operator moves the player, and the gateway sees the move pending from the player's claim.
    let moved = fixture.operate(&cli, "operator:move", "chunk:move_player", &move_to("arena")).await;
    assert_eq!(result::<MovePlayerResult>(&moved), MovePlayerResult {});
    let (_, source) = until(&mut updates, "login", |claim: &GatewayClaim| claim.pending_move.is_some()).await;
    assert_eq!(source.pending_move.map(|pending| pending.operation_id).as_deref(), Some("operator:move"));
    // The gateway claims the destination under the move's operation ID, releases the source, then activates it.
    let claimed = fixture.platform(&gateway, stream, "operator:move", "chunk:claim", &ClaimArguments::default()).await;
    assert!(matches!(result::<ClaimResult>(&claimed).outcome, Some(claim_result::Outcome::Assignment(_))));
    let withdrawn = fixture.platform(&gateway, stream, "login", "chunk:withdraw", &()).await;
    assert!(!result::<WithdrawResult>(&withdrawn).unknown);
    let activated = fixture.platform(&gateway, stream, "operator:move", "chunk:activate", &()).await;
    assert!(!result::<ActivateResult>(&activated).waiting);
    arrival(&mut updates, "operator:move").await;
    drop(updates);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_drain_retires_their_host_and_a_retry_returns_its_first_outcome() {
    let (mut fixture, jvm) = with_jvm().await;
    let host = fixture.arrive().await;
    let cli = fixture.cli.clone();
    let player = drain(Target::Player(runtime::PLAYER.into()), 10);
    let drained = fixture.operate(&cli, "operator:drain", "chunk:drain", &player).await;
    let result: DrainResult = result(&drained);
    assert_eq!(result.host, host);
    assert_eq!(fixture.operate(&cli, "operator:drain", "chunk:drain", &player).await.outcome, drained.outcome);
    let longer = drain(Target::Player(runtime::PLAYER.into()), 20);
    let by_host = drain(Target::Host(host.clone()), 10);
    for changed in [longer, by_host] {
        assert_eq!(
            code(&fixture.operate(&cli, "operator:drain", "chunk:drain", &changed).await),
            Code::OperationMismatch
        );
    }
    let moved = fixture.operate(&cli, "operator:drain", "chunk:move_player", &move_to("arena")).await;
    assert_eq!(code(&moved), Code::OperationMismatch);

    let mut nodes = fixture.watch(&cli, "nodes", None).await;
    let node: Node = value(&next(&mut nodes).await, &host).expect("the host's node");
    assert_eq!((node.phase(), node.drain_deadline_ms), (NodePhase::Draining, result.deadline_ms));
    drop(nodes);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_operator_follows_its_topics_and_calls_its_methods_under_its_ids() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    for credential in [gateway.as_str(), JVM] {
        for topic in ["nodes", "players"] {
            let mut updates = fixture.watch(credential, topic, None).await;
            assert_eq!(next(&mut updates).await.error.map(|error| error.code()), Some(Code::Denied));
        }
        let moved = fixture.operate(credential, "operator:move", "chunk:move_player", &move_to("arena")).await;
        assert_eq!(code(&moved), Code::Denied);
        let arguments = drain(Target::Host("host-1".into()), 0);
        assert_eq!(code(&fixture.operate(credential, "operator:drain", "chunk:drain", &arguments).await), Code::Denied);
    }
    let cli = fixture.cli.clone();
    assert_eq!(code(&fixture.call(&cli, "operator:add", "add", "2").await), Code::Invalid);
    fixture.stop().await;
}
