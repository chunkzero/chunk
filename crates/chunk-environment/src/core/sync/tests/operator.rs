//! The operator's topics and methods over the sync protocol.

use super::{runtime::with_jvm, *};
use chunk_proto::{
    sync::v1::{
        ClaimPhase, DrainArguments, DrainResult, JvmHealth, MovePlayerArguments, MovePlayerResult, Node, NodePhase,
        OperatorPlayer, SessionDemand, drain_arguments::Target,
    },
    v1::{ActivateClaim, ClaimPhase as ControlPhase},
};

impl Fixture {
    async fn watch(&mut self, credential: &str, topic: &str) -> Streaming<Update> {
        let subscription = SubscribeRequest { topic: topic.into(), ..SubscribeRequest::default() };
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nodes_follow_health_and_a_host_drain_until_it_stopped() {
    let (mut fixture, jvm) = with_jvm().await;
    let host = fixture.arrive().await;
    let cli = fixture.cli.clone();
    let mut nodes = fixture.watch(&cli, "nodes").await;
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

    let drained = fixture.operate(&cli, "drain-host", "chunk:drain", &drain(Target::Host(host.clone()), 1)).await;
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
async fn players_follow_claims_and_a_move_queued_under_an_operation_id() {
    let (mut fixture, jvm) = with_jvm().await;
    let cli = fixture.cli.clone();
    let mut players = fixture.watch(&cli, "players").await;
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

    let destination = |key: &str| SessionDemand {
        key: key.into(),
        session_type: "bridge/default".into(),
        machine_profile: "small".into(),
    };
    let arena = MovePlayerArguments { player: runtime::PLAYER.into(), destination: Some(destination("arena")) };
    let moved = fixture.operate(&cli, "move", "chunk:move_player", &arena).await;
    assert_eq!(result::<MovePlayerResult>(&moved), MovePlayerResult {});
    assert!(moved.position.is_some());
    until(&mut players, runtime::PLAYER, |player: &OperatorPlayer| player.moving).await;

    assert_eq!(fixture.operate(&cli, "move", "chunk:move_player", &arena).await.outcome, moved.outcome);
    let elsewhere = MovePlayerArguments { destination: Some(destination("elsewhere")), ..arena.clone() };
    assert_eq!(code(&fixture.operate(&cli, "move", "chunk:move_player", &elsewhere).await), Code::OperationMismatch);
    // The login's claim already holds this operation ID.
    assert_eq!(code(&fixture.operate(&cli, "login", "chunk:move_player", &arena).await), Code::OperationMismatch);
    assert_eq!(code(&fixture.operate(&cli, "prep:1", "chunk:move_player", &arena).await), Code::Invalid);
    drop(players);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_drain_retires_their_host_and_a_retry_returns_its_first_outcome() {
    let (mut fixture, jvm) = with_jvm().await;
    let host = fixture.arrive().await;
    let cli = fixture.cli.clone();
    let player = drain(Target::Player(runtime::PLAYER.into()), 10);
    let drained = fixture.operate(&cli, "drain", "chunk:drain", &player).await;
    let result: DrainResult = result(&drained);
    assert_eq!(result.host, host);
    assert_eq!(fixture.operate(&cli, "drain", "chunk:drain", &player).await.outcome, drained.outcome);
    let longer = drain(Target::Player(runtime::PLAYER.into()), 20);
    assert_eq!(code(&fixture.operate(&cli, "drain", "chunk:drain", &longer).await), Code::OperationMismatch);
    let by_host = drain(Target::Host(host.clone()), 10);
    assert_eq!(code(&fixture.operate(&cli, "drain", "chunk:drain", &by_host).await), Code::OperationMismatch);

    let mut nodes = fixture.watch(&cli, "nodes").await;
    let node: Node = value(&next(&mut nodes).await, &host).expect("the host's node");
    assert_eq!((node.phase(), node.drain_deadline_ms), (NodePhase::Draining, result.deadline_ms));
    drop(nodes);
    fixture.stop().await;
    jvm.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_operator_follows_its_topics_and_calls_its_methods() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    for credential in [gateway.as_str(), JVM] {
        for topic in ["nodes", "players"] {
            let mut updates = fixture.watch(credential, topic).await;
            assert_eq!(next(&mut updates).await.error.map(|error| error.code()), Some(Code::Denied));
        }
        let arguments = MovePlayerArguments { player: runtime::PLAYER.into(), destination: None };
        assert_eq!(code(&fixture.operate(credential, "move", "chunk:move_player", &arguments).await), Code::Denied);
        let arguments = drain(Target::Host("host-1".into()), 0);
        assert_eq!(code(&fixture.operate(credential, "drain", "chunk:drain", &arguments).await), Code::Denied);
    }
    fixture.stop().await;
}
