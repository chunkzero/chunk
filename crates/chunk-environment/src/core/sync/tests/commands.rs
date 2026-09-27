//! A gateway's commands over `chunk:*` calls and the `command/<op>` topic.

use super::*;
use chunk_proto::sync::v1::{
    CommandArguments, CommandEffect, CommandResult, CommandSubscription, CommandsResult, EffectArguments, EffectResult,
    GatewayClaim, PrepareResult, SuggestArguments, SuggestResult, command_effect,
};

/// Core with the player arrived through gateway `proxy`, the gateway's topic stream and its ID, and the fake JVM.
async fn arrived() -> (Fixture, Streaming<Update>, String, JoinHandle<()>) {
    use chunk_proto::v1::ClaimPhase;
    let (jvm, server) = runtime::Runtime::start();
    let mut fixture = Fixture::with_host(Arc::new(jvm)).await;
    fixture.control.activate_release(runtime::release()).unwrap();
    let subscription = SubscribeRequest { topic: "gateway/proxy".into(), ..SubscribeRequest::default() };
    let gateway = fixture.gateway.clone();
    let mut updates = fixture.client.subscribe(authorized(subscription, &gateway)).await.unwrap().into_inner();
    let stream = next(&mut updates).await.stream;
    fixture.control.claim(runtime::login()).await.unwrap();
    let arrived = |fixture: &Fixture| {
        let players = fixture.control.players().unwrap().players;
        players.iter().any(|player| player.phase() == ClaimPhase::Arrived)
    };
    while !arrived(&fixture) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    (fixture, updates, stream, server)
}

/// Calls command method `method` as `credential` on gateway stream `stream`, naming the player unless it's
/// `chunk:effect`.
async fn call(
    mut client: CoreClient<Channel>,
    credential: &str,
    stream: &str,
    operation: &str,
    method: &str,
    arguments: &impl Message,
) -> CallResponse {
    let player = Caller { session: String::new(), player: runtime::PLAYER.into() };
    let message = CallRequest {
        operation_id: operation.into(),
        method: method.into(),
        arguments: arguments.encode_to_vec(),
        caller: (method != "chunk:effect").then_some(player),
        stream: stream.into(),
        ..CallRequest::default()
    };
    client.call(authorized(message, credential)).await.unwrap().into_inner()
}

/// Runs `say` with `input` under prepared `operation`.
async fn say(
    client: CoreClient<Channel>,
    credential: String,
    stream: String,
    operation: String,
    input: &str,
) -> CallResponse {
    let arguments = CommandArguments { command_id: SAY.into(), input: input.into() };
    call(client, &credential, &stream, &operation, "chunk:command", &arguments).await
}

async fn prepare(fixture: &Fixture) -> String {
    let message = CallRequest { method: "chunk:prepare".into(), ..CallRequest::default() };
    let response = fixture.client.clone().call(authorized(message, &fixture.gateway)).await.unwrap().into_inner();
    decoded::<PrepareResult>(&response).operation_id
}

fn decoded<T: Message + Default>(response: &CallResponse) -> T {
    match &response.outcome {
        Some(Outcome::Result(result)) => T::decode(result.as_slice()).unwrap(),
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_reads_the_commands_and_suggestions_its_arrived_player_sees() {
    let (mut fixture, updates, stream, server) = arrived().await;
    let (client, gateway) = (fixture.client.clone(), fixture.gateway.clone());
    let catalog = call(client.clone(), &gateway, &stream, "", "chunk:commands", &()).await;
    let CommandsResult { commands_json, allowed } = decoded(&catalog);
    let commands: serde_json::Value = serde_json::from_slice(&commands_json).unwrap();
    assert_eq!((commands[SAY]["name"].as_str(), allowed.as_slice()), (Some("say"), [SAY.to_owned()].as_slice()));
    let arguments =
        SuggestArguments { command_id: SAY.into(), query: "choices".into(), input: "say o".into(), cursor: 5 };
    let suggested = call(client.clone(), &gateway, &stream, "", "chunk:suggest", &arguments).await;
    assert_eq!(decoded::<SuggestResult>(&suggested).values, ["one", "two"]);

    let cli = fixture.cli.clone();
    assert_eq!(code(&call(client.clone(), &cli, &stream, "", "chunk:commands", &()).await), Code::Denied);
    let other = fixture.gateways.mint("other");
    let subscription = SubscribeRequest { topic: "gateway/other".into(), ..SubscribeRequest::default() };
    let mut foreign = fixture.client.subscribe(authorized(subscription, &other)).await.unwrap().into_inner();
    let foreign_stream = next(&mut foreign).await.stream;
    let refused = call(client, &other, &foreign_stream, "", "chunk:commands", &()).await;
    assert_eq!(code(&refused), Code::Denied);
    drop((updates, foreign));
    fixture.stop().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_commands_message_waits_on_its_topic_until_the_gateway_acknowledges_it() {
    let (mut fixture, updates, stream, server) = arrived().await;
    let (client, gateway) = (fixture.client.clone(), fixture.gateway.clone());
    let operation = prepare(&fixture).await;
    let topic = |stream: &str| SubscribeRequest {
        topic: format!("command/{operation}"),
        arguments: CommandSubscription { stream: stream.into() }.encode_to_vec(),
        ..SubscribeRequest::default()
    };
    let mut effects = fixture.client.subscribe(authorized(topic(&stream), &gateway)).await.unwrap().into_inner();
    let first = next(&mut effects).await;
    assert!(first.snapshot && first.upserts.is_empty() && !first.stream.is_empty());

    let running = tokio::spawn(say(client.clone(), gateway.clone(), stream.clone(), operation.clone(), "say hello"));
    let mut pending = next(&mut effects).await;
    while pending.upserts.is_empty() {
        pending = next(&mut effects).await;
    }
    let [Entry { key, state: Some(State::Value(value)) }] = pending.upserts.as_slice() else {
        panic!("expected one pending effect, got {pending:?}");
    };
    let effect = CommandEffect::decode(value.as_slice()).unwrap().effect;
    assert_eq!(effect, Some(command_effect::Effect::Message("hello".into())));
    let ack = EffectArguments { operation_id: operation.clone(), sequence: key.parse().unwrap(), failed: false };

    // Only the gateway that started the command follows it and acknowledges its effects.
    let other = fixture.gateways.mint("other");
    let subscription = SubscribeRequest { topic: "gateway/other".into(), ..SubscribeRequest::default() };
    let mut foreign = fixture.client.subscribe(authorized(subscription, &other)).await.unwrap().into_inner();
    let foreign_stream = next(&mut foreign).await.stream;
    let mut denied = fixture.client.subscribe(authorized(topic(&foreign_stream), &other)).await.unwrap().into_inner();
    assert_eq!(next(&mut denied).await.error.map(|error| error.code()), Some(Code::Denied));
    let foreign_ack = call(client.clone(), &other, &foreign_stream, "", "chunk:effect", &ack).await;
    assert_eq!(code(&foreign_ack), Code::Denied);

    let acknowledged = call(client.clone(), &gateway, &stream, "", "chunk:effect", &ack).await;
    assert!(!decoded::<EffectResult>(&acknowledged).unknown);
    let finished = running.await.unwrap();
    assert_eq!(decoded::<CommandResult>(&finished).result_json, b"null");

    let mut last = next(&mut effects).await;
    while last.error.is_none() {
        last = next(&mut effects).await;
    }
    assert_eq!(last.error.map(|error| error.code()), Some(Code::Stopped));
    let repeated = call(client.clone(), &gateway, &stream, "", "chunk:effect", &ack).await;
    assert!(decoded::<EffectResult>(&repeated).unknown);
    assert_eq!(say(client, gateway, stream, operation.clone(), "say hello").await, finished);
    drop((updates, foreign));
    fixture.stop().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_queues_a_commands_move_which_the_gateway_sees_on_its_own_topic() {
    let (fixture, mut updates, stream, server) = arrived().await;
    let operation = prepare(&fixture).await;
    let entered = say(fixture.client.clone(), fixture.gateway.clone(), stream, operation, "say enter").await;
    assert_eq!(decoded::<CommandResult>(&entered).result_json, b"null");
    loop {
        let update = next(&mut updates).await;
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
    drop(updates);
    fixture.stop().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_calls_a_commands_session_method_itself() {
    let (fixture, updates, stream, server) = arrived().await;
    let operation = prepare(&fixture).await;
    let called = say(fixture.client.clone(), fixture.gateway.clone(), stream, operation, "say status").await;
    assert_eq!(decoded::<CommandResult>(&called).result_json, b"null");
    drop(updates);
    fixture.stop().await;
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_commands_beyond_sixteen_all_run() {
    let (fixture, updates, stream, server) = arrived().await;
    let mut operations = Vec::new();
    for _ in 0..24 {
        operations.push(prepare(&fixture).await);
    }
    let runs: Vec<_> = operations
        .into_iter()
        .map(|operation| {
            let (client, gateway, stream) = (fixture.client.clone(), fixture.gateway.clone(), stream.clone());
            tokio::spawn(async move { say(client, gateway, stream, operation, "say wait").await })
        })
        .collect();
    for run in runs {
        let response = run.await.unwrap();
        assert_eq!(decoded::<CommandResult>(&response).result_json, b"null");
    }
    drop(updates);
    fixture.stop().await;
    server.abort();
}
