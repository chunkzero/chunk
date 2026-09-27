//! A gateway hook runs within its call.

use super::*;
use chunk_proto::sync::v1::PrepareResult;

const WAIT: &str = "shared/domains/hooks/wait";

/// The fixture's deployment as `hooks`, with `domain.enter` hook `wait`, which adds 1 twice with a second between.
fn hooks() -> Deployment {
    let mut deployment = Deployment {
        id: "hooks".into(),
        source: format!(
            "{SOURCE}\nexport async function wait(ctx) {{ await ctx.runMutation('add', 1); await ctx.sleep(1000); \
             await ctx.runMutation('add', 1); return null; }}"
        ),
        ..deployment()
    };
    let hook = serde_json::json!({"domain": "", "event": "domain.enter", "export": "wait"});
    let domains = deployment.contracts.domains.as_mut().unwrap();
    domains.hooks.insert(WAIT.into(), serde_json::from_value(hook).unwrap());
    deployment
}

/// Calls `method` of `hooks` under `operation` as `credential`, or platform method `chunk:prepare`.
async fn call(mut client: CoreClient<Channel>, credential: String, operation: String, method: &str) -> CallResponse {
    let player = serde_json::json!({"uuid": runtime::PLAYER, "username": "player"});
    let (arguments, deployment) = match method {
        "chunk:prepare" => (String::new(), ""),
        WAIT => (serde_json::json!({"domain": "", "eventId": "wait", "player": player}).to_string(), "hooks"),
        _ => ("null".into(), "hooks"),
    };
    let message = CallRequest {
        operation_id: operation,
        method: method.into(),
        arguments: arguments.into(),
        deployment: deployment.into(),
        ..CallRequest::default()
    };
    client.call(authorized(message, &credential)).await.unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_hook_call_cancels_the_hook_and_its_retry_reports_it_cancelled() {
    let fixture = Fixture::start().await;
    fixture.backend.deploy(hooks()).await.unwrap();
    let gateway = fixture.gateway.clone();
    let prepared = call(fixture.client.clone(), gateway.clone(), String::new(), "chunk:prepare").await;
    let Some(Outcome::Result(prepared)) = prepared.outcome else {
        panic!("expected an operation ID, got {prepared:?}")
    };
    let operation = PrepareResult::decode(prepared.as_slice()).unwrap().operation_id;
    let count = || call(fixture.client.clone(), gateway.clone(), String::new(), "get");

    let first = tokio::spawn(call(fixture.client.clone(), gateway.clone(), operation.clone(), WAIT));
    tokio::time::timeout(Duration::from_secs(5), async {
        while count().await.outcome != Some(Outcome::Result(b"1".to_vec())) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    first.abort();

    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(count().await.outcome, Some(Outcome::Result(b"1".to_vec())));
    let retry = call(fixture.client.clone(), gateway.clone(), operation, WAIT).await;
    let Some(Outcome::Error(error)) = &retry.outcome else { panic!("expected the cancellation, got {retry:?}") };
    assert_eq!(error.code(), Code::Unavailable, "{error:?}");
    assert!(error.message.contains("cancel"), "{error:?}");
    assert_eq!(count().await.outcome, Some(Outcome::Result(b"1".to_vec())));
    fixture.stop().await;
}
