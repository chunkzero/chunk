//! Commands that wait for the backend: for its action capacity, or while it's busy.

use super::*;
use chunk_backend::{ActionHandle, Backend, Call, CommandIdentity};

/// Holds all 24 live actions for a second, or until the returned handles drop, so a command started meanwhile waits
/// for admission.
async fn hold(backend: &Backend) -> Vec<ActionHandle> {
    let mut holds = Vec::new();
    for _ in 0..24 {
        let id = backend.allocate_action_id().await.unwrap();
        holds.push(backend.start_action(id, cli_call("slow", serde_json::json!(0))).await.unwrap());
    }
    holds
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
async fn a_command_start_is_charged_for_its_payload_before_it_waits_for_the_backend() {
    let arrived = arrived().await;
    let (gateway, backend) = (arrived.gateway.clone(), arrived.fixture.backend.clone());
    let operation = gateway.prepare().await;
    // The mutation spins until its execution limit, which holds the backend for a second.
    let spin = tokio::spawn({
        let backend = backend.clone();
        async move { backend.mutate("spin".into(), cli_call("spin", serde_json::Value::Null)).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
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
        async move { next(&mut stale.follow(&operation).await).await }
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
