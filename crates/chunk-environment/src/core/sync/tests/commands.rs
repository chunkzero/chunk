//! A gateway's commands over `chunk:*` calls and the `command/<op>` topic.

use super::*;
use chunk_proto::sync::v1::{
    CommandArguments, CommandEffect, CommandOutcome, CommandStarted, CommandSubscription, CommandsResult,
    EffectArguments, EffectResult, GatewayClaim, PrepareResult, SuggestArguments, SuggestResult, WithdrawResult,
    command_effect, command_outcome,
};

mod admission;

/// Core with the player arrived through gateway `proxy`, whose topic stream it holds, and the fake JVM.
struct Arrived {
    fixture: Fixture,
    updates: Streaming<Update>,
    gateway: Gateway,
    jvm: runtime::Runtime,
    server: JoinHandle<()>,
}

/// A gateway's credential and its current `gateway/<id>` stream.
#[derive(Clone)]
struct Gateway {
    client: CoreClient<Channel>,
    credential: String,
    stream: String,
}

async fn arrived() -> Arrived {
    use chunk_proto::v1::ClaimPhase;
    let (jvm, server) = runtime::Runtime::start();
    let fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    fixture.control.activate_release(runtime::release()).unwrap();
    let (updates, gateway) = Gateway::follow_own(&fixture, fixture.gateway.clone(), "proxy").await;
    fixture.control.claim(runtime::login()).await.unwrap();
    let arrived = |fixture: &Fixture| {
        let players = fixture.control.players().unwrap().players;
        players.iter().any(|player| player.phase() == ClaimPhase::Arrived)
    };
    while !arrived(&fixture) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Arrived { fixture, updates, gateway, jvm, server }
}

impl Arrived {
    /// Another gateway, and its `gateway/<id>` stream.
    async fn other(&self) -> (Gateway, Streaming<Update>) {
        let (updates, other) = Gateway::follow_own(&self.fixture, self.fixture.gateways.mint("other"), "other").await;
        (other, updates)
    }

    async fn stop(self) {
        drop(self.updates);
        self.fixture.stop().await;
        self.server.abort();
    }
}

impl Gateway {
    /// Follows `gateway/<id>` as `credential`.
    async fn follow_own(fixture: &Fixture, credential: String, id: &str) -> (Streaming<Update>, Self) {
        let mut client = fixture.client.clone();
        let subscription = SubscribeRequest { topic: format!("gateway/{id}"), ..SubscribeRequest::default() };
        let mut updates = client.subscribe(authorized(subscription, &credential)).await.unwrap().into_inner();
        let stream = next(&mut updates).await.stream;
        (updates, Self { client, credential, stream })
    }

    async fn call(&self, operation: &str, method: &str, arguments: &impl Message) -> CallResponse {
        call(self.client.clone(), &self.credential, &self.stream, operation, method, arguments, runtime::PLAYER).await
    }

    async fn prepare(&self) -> String {
        let message = CallRequest { method: "chunk:prepare".into(), ..CallRequest::default() };
        let response = self.client.clone().call(authorized(message, &self.credential)).await.unwrap();
        decoded::<PrepareResult>(&response.into_inner()).operation_id
    }

    /// Starts `command` with `input` for `player` under prepared `operation`.
    async fn start(&self, operation: &str, command: &str, input: &str, player: &str) -> CallResponse {
        let arguments = CommandArguments { command_id: command.into(), input: input.into() };
        let (client, credential, stream) = (self.client.clone(), &self.credential, &self.stream);
        call(client, credential, stream, operation, "chunk:command", &arguments, player).await
    }

    /// Starts `say` with `input` under prepared `operation`.
    async fn say(&self, operation: &str, input: &str) -> CallResponse {
        self.start(operation, SAY, input, runtime::PLAYER).await
    }

    /// Follows the command under `operation`.
    async fn follow(&self, operation: &str) -> Streaming<Update> {
        let topic = SubscribeRequest {
            topic: format!("command/{operation}"),
            arguments: CommandSubscription { stream: self.stream.clone() }.encode_to_vec(),
            ..SubscribeRequest::default()
        };
        self.client.clone().subscribe(authorized(topic, &self.credential)).await.unwrap().into_inner()
    }

    /// Starts `say` with `input`, then follows it to its outcome.
    async fn run(&self, input: &str) -> CommandOutcome {
        let operation = self.prepare().await;
        decoded::<CommandStarted>(&self.say(&operation, input).await);
        outcome(&mut self.follow(&operation).await).await
    }
}

/// Calls platform method `method` as `credential` on gateway stream `stream`, naming `player` for the methods that take
/// one.
async fn call(
    mut client: CoreClient<Channel>,
    credential: &str,
    stream: &str,
    operation: &str,
    method: &str,
    arguments: &impl Message,
    player: &str,
) -> CallResponse {
    let player = Caller { session: String::new(), player: player.into() };
    let message = CallRequest {
        operation_id: operation.into(),
        method: method.into(),
        arguments: arguments.encode_to_vec(),
        caller: (!matches!(method, "chunk:effect" | "chunk:withdraw")).then_some(player),
        stream: stream.into(),
        ..CallRequest::default()
    };
    client.call(authorized(message, credential)).await.unwrap().into_inner()
}

/// Reads a command's topic to its outcome, which must end it.
async fn outcome(effects: &mut Streaming<Update>) -> CommandOutcome {
    loop {
        let update = next(effects).await;
        assert!(update.error.is_none(), "the topic failed: {update:?}");
        if let Some(Entry { state: Some(State::Value(value)), .. }) =
            update.upserts.iter().find(|entry| entry.key == "outcome")
        {
            assert!(effects.message().await.unwrap().is_none());
            return CommandOutcome::decode(value.as_slice()).unwrap();
        }
    }
}

fn returned(outcome: &CommandOutcome) -> &[u8] {
    match &outcome.outcome {
        Some(command_outcome::Outcome::ResultJson(json)) => json,
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

fn failed(outcome: &CommandOutcome) -> bool {
    matches!(outcome.outcome, Some(command_outcome::Outcome::Error(_)))
}

fn decoded<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

fn failure(update: &Update) -> Option<Code> {
    update.error.as_ref().map(chunk_proto::sync::v1::Error::code)
}

/// Waits up to 10 seconds for `condition`.
async fn until(condition: impl Fn() -> bool) {
    let met = async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), met).await.expect("the condition held in time");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_reads_the_commands_and_suggestions_its_arrived_player_sees() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    // `say`'s permission query allows only the player's gateway caller.
    let CommandsResult { commands_json, allowed } = decoded(&gateway.call("", "chunk:commands", &()).await);
    let commands: serde_json::Value = serde_json::from_slice(&commands_json).unwrap();
    assert_eq!((commands[SAY]["name"].as_str(), allowed.as_slice()), (Some("say"), [SAY.to_owned()].as_slice()));
    let arguments =
        SuggestArguments { command_id: SAY.into(), query: "choices".into(), input: "say o".into(), cursor: 5 };
    let suggested = gateway.call("", "chunk:suggest", &arguments).await;
    assert_eq!(decoded::<SuggestResult>(&suggested).values, ["one", "two"]);

    let cli = Gateway { credential: arrived.fixture.cli.clone(), ..gateway.clone() };
    assert_eq!(code(&cli.call("", "chunk:commands", &()).await), Code::Denied);
    let (other, foreign) = arrived.other().await;
    assert_eq!(code(&other.call("", "chunk:commands", &()).await), Code::Denied);
    drop(foreign);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commands_message_waits_on_its_topic_until_the_gateway_acknowledges_it() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say hello").await);
    let mut effects = gateway.follow(&operation).await;
    let mut pending = next(&mut effects).await;
    assert!(pending.snapshot && !pending.stream.is_empty());
    while pending.upserts.is_empty() {
        pending = next(&mut effects).await;
    }
    let [Entry { key, state: Some(State::Value(value)) }] = pending.upserts.as_slice() else {
        panic!("expected one pending effect, got {pending:?}");
    };
    let effect = CommandEffect::decode(value.as_slice()).unwrap().effect;
    assert_eq!(effect, Some(command_effect::Effect::Message("hello".into())));
    let ack = EffectArguments { operation_id: operation.clone(), sequence: key.parse().unwrap(), failed: false };

    // Only the gateway that started the command follows it, acknowledges its effects and retries it.
    let (other, foreign) = arrived.other().await;
    assert_eq!(failure(&next(&mut other.follow(&operation).await).await), Some(Code::Denied));
    assert_eq!(code(&other.call("", "chunk:effect", &ack).await), Code::Denied);
    assert_eq!(code(&other.say(&operation, "say hello").await), Code::Denied);

    let acknowledged = gateway.call("", "chunk:effect", &ack).await;
    assert!(!decoded::<EffectResult>(&acknowledged).unknown);
    assert_eq!(returned(&outcome(&mut effects).await), b"null");
    assert!(decoded::<EffectResult>(&gateway.call("", "chunk:effect", &ack).await).unknown);

    // Once it finished, a retry reports it started, and following it again reports its retained outcome.
    decoded::<CommandStarted>(&gateway.say(&operation, "say hello").await);
    assert_eq!(returned(&outcome(&mut gateway.follow(&operation).await).await), b"null");
    drop(foreign);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rejected_command_leaves_nothing_to_follow() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    let arguments = CommandArguments { command_id: "scopes/commands/missing".into(), input: "missing".into() };
    let rejected = gateway.call(&operation, "chunk:command", &arguments).await;
    assert!(matches!(rejected.outcome, Some(Outcome::Error(_))));
    assert_eq!(failure(&next(&mut gateway.follow(&operation).await).await), Some(Code::Invalid));
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_is_cancelled_once_its_topic_closes() {
    let mut arrived = arrived().await;
    let gateway = arrived.gateway.clone();
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say write").await);
    let mut effects = gateway.follow(&operation).await;
    next(&mut effects).await;
    drop(effects);
    tokio::time::sleep(Duration::from_millis(600)).await;
    let cli = arrived.fixture.cli.clone();
    assert_eq!(arrived.fixture.call(&cli, "", "get", "null").await.outcome, Some(Outcome::Result(b"0".to_vec())));
    assert!(failed(&outcome(&mut gateway.follow(&operation).await).await));
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_commands_queued_session_call_never_runs() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say queued").await);
    let effects = gateway.follow(&operation).await;
    while arrived.jvm.methods().1 == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    drop(effects);
    while arrived.jvm.methods().2 == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(arrived.jvm.methods().0, 0);
    assert!(failed(&outcome(&mut gateway.follow(&operation).await).await));
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_that_changes_the_command_input_or_player_is_a_mismatch() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    let mut effects = gateway.follow(&operation).await;
    let changed = [
        ("scopes/commands/other", "say wait", runtime::PLAYER),
        (SAY, "say hello", runtime::PLAYER),
        (SAY, "say wait", "00000000-0000-0000-0000-000000000002"),
    ];
    for finished in [false, true] {
        if finished {
            assert_eq!(returned(&outcome(&mut effects).await), b"null");
        }
        for (command, input, player) in changed {
            let retried = gateway.start(&operation, command, input, player).await;
            assert_eq!(code(&retried), Code::OperationMismatch, "{command} {input} {player}, finished: {finished}");
        }
    }
    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_subscription_supersedes_the_one_following_a_command() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    let mut first = gateway.follow(&operation).await;
    next(&mut first).await;
    let mut second = gateway.follow(&operation).await;
    next(&mut second).await;
    assert_eq!(failure(&next(&mut first).await), Some(Code::Stopped));
    assert!(first.message().await.unwrap().is_none());
    assert_eq!(returned(&outcome(&mut second).await), b"null");
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_that_returned_runs_until_its_session_call_settles_and_closing_its_topic_cancels_the_call() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say detached").await);
    let mut effects = gateway.follow(&operation).await;
    next(&mut effects).await;
    until(|| arrived.jvm.methods().1 > 0).await;
    // The handler returns 200 ms after the call; the call keeps the command and its topic open.
    let quiet = tokio::time::timeout(Duration::from_millis(600), effects.message()).await;
    assert!(quiet.is_err(), "the command finished while its session call was queued");
    drop(effects);
    until(|| arrived.jvm.methods().2 > 0).await;
    assert_eq!(arrived.jvm.methods().0, 0);
    assert_eq!(returned(&outcome(&mut gateway.follow(&operation).await).await), b"null");
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_stopping_cancels_a_sleeping_command_before_it_writes() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say write").await);
    // Core stops within the grace a command waits for its first subscription.
    arrived.fixture.stop.cancel();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let get = chunk_backend::Call {
        deployment: chunk_js::DeploymentId::new("test").unwrap(),
        function: "get".into(),
        arguments: serde_json::Value::Null.into(),
        caller: serde_json::json!({"kind": "cli"}).into(),
    };
    assert_eq!(&*arrived.fixture.backend.query(get).await.unwrap().json, "0");
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retry_after_the_players_claim_moved_on_reports_the_command_it_started() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    let mut effects = gateway.follow(&operation).await;
    let withdrawn = gateway.call("login", "chunk:withdraw", &()).await;
    assert!(!decoded::<WithdrawResult>(&withdrawn).unknown);
    assert_eq!(code(&gateway.say(&gateway.prepare().await, "say wait").await), Code::Denied);

    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    assert_eq!(returned(&outcome(&mut effects).await), b"null");
    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_queues_a_commands_move_which_the_gateway_sees_on_its_own_topic() {
    let mut arrived = arrived().await;
    assert_eq!(returned(&arrived.gateway.run("say enter").await), b"null");
    loop {
        let update = next(&mut arrived.updates).await;
        let destination = update.upserts.iter().find_map(|entry| match &entry.state {
            Some(State::Value(value)) if entry.key == "login" => {
                GatewayClaim::decode(value.as_slice()).unwrap().pending_move.and_then(|moved| moved.destination)
            }
            _ => None,
        });
        if let Some(destination) = destination {
            assert_eq!(destination.key, "arena");
            break;
        }
    }
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_calls_a_commands_session_method_itself() {
    let arrived = arrived().await;
    assert_eq!(returned(&arrived.gateway.run("say status").await), b"null");
    assert_eq!(arrived.jvm.methods().0, 1);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_spelling_of_a_live_commands_operation_id_is_invalid() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, "say wait").await);
    let (prefix, sequence) = operation.rsplit_once(':').unwrap();
    let alias = format!("{prefix}:0{sequence}");
    assert_eq!(code(&gateway.say(&alias, "say wait").await), Code::Invalid);
    assert_eq!(failure(&next(&mut gateway.follow(&alias).await).await), Some(Code::Invalid));
    let ack = EffectArguments { operation_id: alias, sequence: 1, failed: false };
    assert_eq!(code(&gateway.call("", "chunk:effect", &ack).await), Code::Invalid);
    assert_eq!(returned(&outcome(&mut gateway.follow(&operation).await).await), b"null");
    arrived.stop().await;
}
