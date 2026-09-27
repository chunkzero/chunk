//! Effectful calls under operation IDs `chunk:prepare` issued, and the gateway's hooks.

use super::*;
use chunk_proto::sync::v1::{ManifestResult, PrepareResult};

impl Fixture {
    /// Runs platform method `method` as `credential` in `deployment`, with no arguments.
    async fn platform_call(&mut self, credential: &str, method: &str, deployment: &str) -> CallResponse {
        let message = CallRequest { method: method.into(), deployment: deployment.into(), ..CallRequest::default() };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn prepare(&mut self, credential: &str) -> String {
        match self.platform_call(credential, "chunk:prepare", "").await.outcome {
            Some(Outcome::Result(result)) => PrepareResult::decode(result.as_slice()).unwrap().operation_id,
            outcome => panic!("expected an operation ID, got {outcome:?}"),
        }
    }
}

fn login() -> String {
    serde_json::json!({
        "domain": "", "eventId": "login", "destination": null,
        "player": {"uuid": runtime::PLAYER, "username": "player"}
    })
    .to_string()
}

/// Runs action `slow`, which adds 2 twice with a second between, under `operation` as `credential`.
async fn slow(mut client: CoreClient<Channel>, credential: String, operation: String) -> CallResponse {
    let message = CallRequest {
        operation_id: operation,
        method: "slow".into(),
        arguments: "2".into(),
        deployment: "test".into(),
        ..CallRequest::default()
    };
    client.call(authorized(message, &credential)).await.unwrap().into_inner()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prepared_effects_run_once_and_retries_return_their_outcomes_after_the_deployment_retires_until_evicted() {
    let mut fixture = Fixture::start().await;
    let (cli, gateway) = (fixture.cli.clone(), fixture.gateway.clone());
    let operation = fixture.prepare(&cli).await;
    let first = fixture.call(&cli, &operation, "bump", "2").await;
    assert_eq!(first.outcome, Some(Outcome::Result(b"2".to_vec())));
    assert_eq!(fixture.call(&cli, &operation, "bump", "2").await, first);
    assert_eq!(fixture.call(&cli, "", "get", "null").await.outcome, Some(Outcome::Result(b"2".to_vec())));
    let hook = fixture.prepare(&gateway).await;
    let admitted = fixture.call(&gateway, &hook, LOGIN, &login()).await;
    assert!(matches!(admitted.outcome, Some(Outcome::Result(_))), "{admitted:?}");

    fixture.backend.deploy(Deployment { id: "filler".into(), ..deployment() }).await.unwrap();
    assert!(fixture.backend.release(DeploymentId::new("test").unwrap()).await.unwrap());
    assert_eq!(fixture.call(&cli, &operation, "bump", "2").await, first);
    assert_eq!(fixture.call(&gateway, &hook, LOGIN, &login()).await, admitted);
    assert_eq!(code(&fixture.call(&cli, &operation, "bump", "3").await), Code::OperationMismatch);

    // Newer outcomes past the retention budget evict both.
    let fill = Call {
        deployment: DeploymentId::new("filler").unwrap(),
        function: "fill".into(),
        arguments: serde_json::json!("x".repeat(1_000_000)).into(),
        caller: serde_json::json!({"kind": "cli"}).into(),
    };
    for _ in 0..35 {
        let id = fixture.backend.allocate_action_id().await.unwrap();
        fixture.backend.start_action(id, fill.clone()).await.unwrap().outcome().await.unwrap();
    }
    assert_eq!(code(&fixture.call(&cli, &operation, "bump", "2").await), Code::OutcomeUnknown);
    assert_eq!(code(&fixture.call(&gateway, &hook, LOGIN, &login()).await), Code::OutcomeUnknown);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_core_without_the_deployment_reports_earlier_effects_unknown() {
    let mut fixture = Fixture::start().await;
    let (cli, gateway) = (fixture.cli.clone(), fixture.gateway.clone());
    let operation = fixture.prepare(&cli).await;
    assert_eq!(fixture.call(&cli, &operation, "bump", "1").await.outcome, Some(Outcome::Result(b"1".to_vec())));
    let hook = fixture.prepare(&gateway).await;
    assert!(matches!(fixture.call(&gateway, &hook, LOGIN, &login()).await.outcome, Some(Outcome::Result(_))));
    fixture.stop().await;

    let mut fixture = Fixture::start().await;
    assert!(fixture.backend.release(DeploymentId::new("test").unwrap()).await.unwrap());
    let (cli, gateway) = (fixture.cli.clone(), fixture.gateway.clone());
    assert_eq!(code(&fixture.call(&cli, &operation, "bump", "1").await), Code::OutcomeUnknown);
    assert_eq!(code(&fixture.call(&gateway, &hook, LOGIN, &login()).await), Code::OutcomeUnknown);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_action_keeps_running_and_concurrent_retries_share_its_one_outcome() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let operation = fixture.prepare(&cli).await;
    let first = tokio::spawn(slow(fixture.client.clone(), cli.clone(), operation.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.call(&cli, "", "get", "null").await.outcome != Some(Outcome::Result(b"2".to_vec())) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    first.abort();

    let retry = || slow(fixture.client.clone(), cli.clone(), operation.clone());
    let (one, other) = tokio::join!(retry(), retry());
    assert_eq!(one.outcome, Some(Outcome::Result(b"4".to_vec())));
    assert_eq!(other, one);
    assert_eq!(retry().await, one);
    assert_eq!(fixture.call(&cli, "", "get", "null").await.outcome, Some(Outcome::Result(b"4".to_vec())));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn effects_require_prepared_operation_ids_which_mutations_reject() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let prepared = fixture.prepare(&cli).await;
    assert_eq!(code(&fixture.call(&cli, "not-prepared", "bump", "1").await), Code::Invalid);
    let (incarnation, _) = prepared.rsplit_once(':').unwrap();
    let unissued = format!("{incarnation}:{}", u64::MAX);
    for operation in ["prep:not-prepared", "prep:00000000-0000-0000-0000-000000000000:1", &unissued] {
        assert_eq!(code(&fixture.call(&cli, operation, "bump", "1").await), Code::OutcomeUnknown, "{operation}");
    }

    assert_eq!(code(&fixture.call(&cli, &prepared, "add", "1").await), Code::Invalid);
    assert_eq!(fixture.call(&cli, &prepared, "bump", "1").await.outcome, Some(Outcome::Result(b"1".to_vec())));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_without_the_players_claim_runs_a_hook_as_itself() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    let operation = fixture.prepare(&gateway).await;
    let result = fixture.call(&gateway, &operation, LOGIN, &login()).await;
    let Some(Outcome::Result(result)) = result.outcome else { panic!("expected a result, got {result:?}") };
    let result: serde_json::Value = serde_json::from_slice(&result).unwrap();
    assert_eq!(result, serde_json::json!({"allow": true, "reason": r#"{"kind":"gateway"}"#}));

    let cli = fixture.cli.clone();
    let operation = fixture.prepare(&cli).await;
    assert_eq!(code(&fixture.call(&cli, &operation, LOGIN, &login()).await), Code::Denied);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chunk_manifest_returns_the_current_releases_domain_manifest() {
    let mut fixture = Fixture::start().await;
    fixture.control.activate_release(runtime::release()).unwrap();
    let gateway = fixture.gateway.clone();
    let response = fixture.platform_call(&gateway, "chunk:manifest", "").await;
    let Some(Outcome::Result(result)) = response.outcome else { panic!("expected a result, got {response:?}") };
    let ManifestResult { deployment, manifest_json } = ManifestResult::decode(result.as_slice()).unwrap();
    assert_eq!(deployment, "test");
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_json).unwrap();
    assert_eq!(manifest["hooks"][LOGIN]["event"], "player.login");
    fixture.stop().await;
}
