use super::*;
use chunk_proto::sync::v1::JvmMethodPhase;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn captured_methods_require_live_authority_and_keep_operation_identity_across_retries() {
    let mut fixture = Fixture::new();
    fixture.release.contracts.session_methods = Some(method_contract());
    let control = fixture.control().await;
    let claim = request("method-caller", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(claim.clone()).await.unwrap();
    let identity = assignment.claim.clone().unwrap();
    assert!(control.capture_session(&identity).is_err());
    fixture.arrive(&control, &claim.operation_id).await;
    control.activate(identity.clone()).await.unwrap();
    let captured = control.capture_session(&identity).unwrap();
    let mut wrong = identity.clone();
    wrong.membership_generation += 1;
    assert!(control.capture_session(&wrong).is_err());
    let timeout = Duration::from_secs(10);
    assert!(control.prepare_session_method(&captured, "hidden", json!({}), timeout).is_err());
    assert!(control.prepare_session_method(&captured, "score", json!({"authority":"admin"}), timeout).is_err());
    let operation = control.prepare_session_method(&captured, "score", json!({}), timeout).unwrap();
    let result = control.call_session_method(&operation, &CancellationToken::new()).await.unwrap();
    assert_eq!(result.phase, Some(JvmMethodPhase::Completed));
    assert_eq!(result.result_json, "7");
    assert_eq!(result, control.call_session_method(&operation, &CancellationToken::new()).await.unwrap());
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 1);
    drop(control);
    let control = fixture.control().await;
    let next = control.prepare_session_method(&captured, "score", json!({}), timeout).unwrap();
    assert_ne!(next.operation_id(), operation.operation_id());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(control.call_session_method(&next, &cancelled).await.unwrap().phase, Some(JvmMethodPhase::Cancelled));
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 1);
    control.cancel(claim).await.unwrap();
    assert!(control.capture_session(&identity).is_err());
    assert!(control.prepare_session_method(&captured, "score", json!({}), timeout).is_err());
    fixture.close().await;
}

#[tokio::test]
async fn pinned_method_configuration_validates_declarations_and_keeps_empty_compatibility() {
    let mut fixture = Fixture::new();
    let serialized = serde_json::to_value(&fixture.release).unwrap();
    assert!(serialized.get("session_methods").is_none());
    let decoded: Release = serde_json::from_value(serialized).unwrap();
    assert!(decoded.contracts.session_methods.is_none());
    let mut methods = method_contract();
    fixture.release.contracts.session_methods = Some(methods.clone());
    assert!(fixture.release.validate().is_ok());
    methods.methods[0].session = "missing".into();
    fixture.release.contracts.session_methods = Some(methods);
    assert!(fixture.release.validate().is_err());
    fixture.close().await;
}

pub(super) fn method_contract() -> chunk_contract::SessionMethods {
    serde_json::from_value(json!({"version":1,"methods":[{"app":"bridge","session":"default","name":"score","arguments":{"type":"object","fields":{}},"result":{"type":"integer"}}]})).unwrap()
}
