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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prepared_action_runs_once_and_its_retry_returns_the_outcome() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let operation = fixture.prepare(&cli).await;
    let first = fixture.call(&cli, &operation, "bump", "2").await;
    assert_eq!(first.outcome, Some(Outcome::Result(b"2".to_vec())));
    assert_eq!(fixture.call(&cli, &operation, "bump", "2").await, first);
    assert_eq!(code(&fixture.call(&cli, &operation, "bump", "3").await), Code::OperationMismatch);
    assert_eq!(fixture.call(&cli, "", "get", "null").await.outcome, Some(Outcome::Result(b"2".to_vec())));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn operation_ids_core_did_not_prepare_have_unknown_outcomes() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let prepared = fixture.prepare(&cli).await;
    let (incarnation, _) = prepared.rsplit_once(':').unwrap();
    let unissued = format!("{incarnation}:{}", u64::MAX);
    for operation in ["not-prepared", "00000000-0000-0000-0000-000000000000:1", &unissued] {
        assert_eq!(code(&fixture.call(&cli, operation, "bump", "1").await), Code::OutcomeUnknown, "{operation}");
    }
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
