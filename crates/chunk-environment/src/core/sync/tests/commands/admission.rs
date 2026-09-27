//! Commands that wait for the backend: for its action capacity, or while it's busy.

use super::*;
use chunk_backend::{ActionHandle, Backend, Call, CommandIdentity};
use tonic::transport::Endpoint;

/// The backend's request memory.
const REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// What a `say big` outcome holds at least: its error, cut at 64 KiB.
const BIG: usize = 64 * 1024;

/// Holds all 24 live actions for a second without writing, so a command started meanwhile waits for admission and
/// none of their commits is pending when it's admitted.
async fn hold(backend: &Backend) -> Vec<ActionHandle> {
    let mut holds = Vec::new();
    for _ in 0..24 {
        let id = backend.allocate_action_id().await.unwrap();
        holds.push(backend.start_action(id, cli_call("nap", serde_json::json!(0))).await.unwrap());
    }
    holds
}

/// Holds the backend with a mutation that spins until its execution limit, for a second.
async fn busy(backend: &Backend) -> JoinHandle<chunk_backend::Result<chunk_backend::Update>> {
    let spin = tokio::spawn({
        let backend = backend.clone();
        async move { backend.mutate("spin".into(), cli_call("spin", serde_json::Value::Null)).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    spin
}

fn cli_call(function: &str, arguments: serde_json::Value) -> Call {
    Call {
        deployment: chunk_js::DeploymentId::new("test").unwrap(),
        function: function.into(),
        arguments: arguments.into(),
        caller: serde_json::json!({"kind": "cli"}).into(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_starts_beyond_four_waiting_for_admission_get_unavailable() {
    let arrived = arrived().await;
    let gateway = &arrived.gateway;
    // These hold all 24 live actions for a second, so the next start waits for admission.
    let holds: Vec<_> = (0..24)
        .map(|_| {
            let gateway = gateway.clone();
            tokio::spawn(
                async move { decoded::<CommandStarted>(&gateway.say(&gateway.prepare().await, "say hold").await) },
            )
        })
        .collect();
    for hold in holds {
        hold.await.unwrap();
    }
    let operation = gateway.prepare().await;
    let starts: Vec<_> = (0..6)
        .map(|_| {
            let (gateway, operation) = (gateway.clone(), operation.clone());
            tokio::spawn(async move { gateway.say(&operation, "say wait").await })
        })
        .collect();
    let mut unavailable = 0;
    for start in starts {
        let response = start.await.unwrap();
        if !matches!(response.outcome, Some(Outcome::Result(_))) {
            assert_eq!(code(&response), Code::Unavailable);
            unavailable += 1;
        }
    }
    assert_eq!(unavailable, 1);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_burst_of_commands_beyond_sixteen_all_run() {
    let arrived = arrived().await;
    let runs: Vec<_> = (0..24)
        .map(|_| {
            let gateway = arrived.gateway.clone();
            tokio::spawn(async move { returned(&gateway.run("say wait").await).to_vec() })
        })
        .collect();
    for run in runs {
        assert_eq!(run.await.unwrap(), b"null");
    }
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_stopping_while_a_command_waits_for_admission_never_starts_it() {
    let arrived = arrived().await;
    let (gateway, backend) = (arrived.gateway.clone(), arrived.fixture.backend.clone());
    let holds = hold(&backend).await;
    let operation = gateway.prepare().await;
    let start = tokio::spawn({
        let (gateway, operation) = (gateway.clone(), operation.clone());
        async move { gateway.say(&operation, "say write").await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    arrived.fixture.stop.cancel();
    assert_eq!(code(&start.await.unwrap()), Code::Unavailable);

    for mut hold in holds {
        hold.outcome().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let id = operation.strip_prefix("prep:").unwrap().parse().unwrap();
    let identity = backend.command_identity(id, &arrived.fixture.gateway, None).await.unwrap();
    assert!(matches!(identity, CommandIdentity::Unused), "the command started: {identity:?}");
    assert_eq!(&*backend.query(cli_call("get", serde_json::Value::Null)).await.unwrap().json, "0");
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_a_subscription_while_its_command_waits_for_admission_never_starts_it() {
    let arrived = arrived().await;
    let (gateway, backend) = (arrived.gateway.clone(), arrived.fixture.backend.clone());
    let holds = hold(&backend).await;
    let operation = gateway.prepare().await;
    let topic = gateway.follow(&operation).await;
    let start = tokio::spawn({
        let (gateway, operation) = (gateway.clone(), operation.clone());
        async move { gateway.say(&operation, "say write").await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(topic);
    assert_eq!(code(&start.await.unwrap()), Code::Stopped);

    for mut hold in holds {
        hold.outcome().await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let id = operation.strip_prefix("prep:").unwrap().parse().unwrap();
    let identity = backend.command_identity(id, &arrived.fixture.gateway, None).await.unwrap();
    assert!(matches!(identity, CommandIdentity::Unused), "the command started: {identity:?}");
    assert_eq!(&*backend.query(cli_call("get", serde_json::Value::Null)).await.unwrap().json, "0");
    assert_eq!(code(&gateway.say(&operation, "say write").await), Code::Stopped);
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_start_is_charged_for_its_payload_before_it_waits_for_the_backend() {
    let arrived = arrived().await;
    let (gateway, backend) = (arrived.gateway.clone(), arrived.fixture.backend.clone());
    let operation = gateway.prepare().await;
    let spin = busy(&backend).await;
    let idle = backend.request_bytes();
    let input = format!("say {}", "x".repeat(512 * 1024));
    let start = tokio::spawn(async move { gateway.say(&operation, &input).await });
    until(|| backend.request_bytes() >= idle + 512 * 1024).await;
    assert!(!spin.is_finished(), "the start was charged only once the backend was free");
    assert!(backend.request_bytes() < idle + 2 * 512 * 1024, "the start was charged twice");
    start.await.unwrap();
    assert!(spin.await.unwrap().is_err());
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscription_from_a_superseded_gateway_stream_never_displaces_the_current_one() {
    let arrived = arrived().await;
    let (stale, backend) = (arrived.gateway.clone(), arrived.fixture.backend.clone());
    let holds = hold(&backend).await;
    let operation = stale.prepare().await;
    let start = tokio::spawn({
        let (gateway, operation) = (stale.clone(), operation.clone());
        async move { gateway.say(&operation, "say wait").await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let superseded = tokio::spawn({
        let operation = operation.clone();
        async move {
            let mut topic = stale.follow(&operation).await;
            assert!(next(&mut topic).await.upserts.is_empty()); // The reservation holds.
            next(&mut topic).await
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (_updates, current) = Gateway::follow_own(&arrived.fixture, arrived.gateway.credential.clone(), "proxy").await;
    let following = tokio::spawn({
        let (current, operation) = (current.clone(), operation.clone());
        async move { current.follow(&operation).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    drop(holds);

    decoded::<CommandStarted>(&start.await.unwrap());
    assert_eq!(failure(&superseded.await.unwrap()), Some(Code::Stopped));
    assert_eq!(returned(&outcome(&mut following.await.unwrap()).await), b"null");
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_suggestion_is_charged_for_its_input_before_it_waits_for_the_backend() {
    let arrived = arrived().await;
    let (gateway, backend) = (arrived.gateway.clone(), arrived.fixture.backend.clone());
    let spin = busy(&backend).await;
    let idle = backend.request_bytes();
    let input = format!("say {}", "x".repeat(512 * 1024));
    let arguments = SuggestArguments { command_id: SAY.into(), query: "choices".into(), input, cursor: 5 };
    let suggest = tokio::spawn(async move { gateway.call("", "chunk:suggest", &arguments).await });
    until(|| backend.request_bytes() >= idle + 512 * 1024).await;
    assert!(!spin.is_finished(), "the suggestion was charged only once the backend was free");
    suggest.await.unwrap();
    assert!(spin.await.unwrap().is_err());
    arrived.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_command_subscriptions_stay_charged_until_their_streams_drop() {
    let arrived = arrived().await;
    let (gateway, backend) = (&arrived.gateway, arrived.fixture.backend.clone());
    let stalled = stalled(&arrived).await;
    let idle = backend.request_bytes();
    let finished = start(gateway, "say big").await;
    assert!(failed(&outcome(&mut gateway.follow(&finished).await).await));

    // Commands that finish while followed hold their outcomes, and their topics a copy, until their streams drop.
    let mut streams = Vec::new();
    for _ in 0..2 {
        streams.push(stalled.follow(&start(gateway, "say big").await).await);
    }
    until(|| backend.request_bytes() >= idle + 4 * BIG).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(backend.request_bytes() >= idle + 4 * BIG, "outcomes were released while their streams stalled");
    // Leaves room for eight more outcomes.
    let filler = backend.charge_request(REQUEST_BYTES - backend.request_bytes() - 1024 - 8 * BIG).unwrap();
    // Once all its effects are pending, each subscription replaces the one before, whose effects its client has yet to
    // take.
    let pending = start(gateway, "say pending").await;
    let mut all = gateway.follow(&pending).await;
    while next(&mut all).await.upserts.len() < 6 {}
    streams.push(stalled.follow(&pending).await);
    assert_eq!(failure(&next(&mut all).await), Some(Code::Stopped));
    while backend.request_bytes() + 2 * BIG <= REQUEST_BYTES {
        let held = backend.request_bytes();
        streams.push(stalled.follow(&pending).await);
        until(|| backend.request_bytes() >= held + BIG).await;
    }
    // Following a finished command needs room for its outcome twice.
    let mut rejected = gateway.follow(&finished).await;
    assert_eq!(failure(&next(&mut rejected).await), Some(Code::Overloaded));

    drop((streams, filler));
    until(|| backend.request_bytes() < idle + BIG).await;
    arrived.stop().await;
}

#[tokio::test]
async fn reservations_closed_before_their_commands_started_stay_charged_until_forgotten() {
    let fixture = Fixture::start().await;
    let (updates, gateway) = Gateway::follow_own(&fixture, fixture.gateway.clone(), "proxy").await;
    let backend = fixture.backend.clone();
    let idle = backend.request_bytes();
    for _ in 0..32 {
        let mut topic = gateway.follow(&gateway.prepare().await).await;
        assert!(next(&mut topic).await.upserts.is_empty());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let records = idle + 32 * crate::core::sync::runs::RECORD;
    assert!(backend.request_bytes() >= records, "closed reservations were released at once");

    tokio::time::pause();
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert!(backend.request_bytes() >= records, "closed reservations were released early");
    tokio::time::sleep(Duration::from_secs(2)).await;
    until(|| backend.request_bytes() <= idle).await;
    tokio::time::resume();
    drop(updates);
    fixture.stop().await;
}

/// The arrived gateway on a client that never reads, whose streams have no room.
async fn stalled(arrived: &Arrived) -> Gateway {
    let endpoint = Endpoint::from_shared(arrived.fixture.endpoint.clone()).unwrap();
    let channel = endpoint.initial_stream_window_size(Some(0)).connect().await.unwrap();
    Gateway { client: CoreClient::new(channel), ..arrived.gateway.clone() }
}

/// Starts `input`, returning its operation ID.
async fn start(gateway: &Gateway, input: &str) -> String {
    let operation = gateway.prepare().await;
    decoded::<CommandStarted>(&gateway.say(&operation, input).await);
    operation
}
