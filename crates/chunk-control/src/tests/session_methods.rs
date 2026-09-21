use super::*;
use chunk_proto::v1::{
    SessionMethodPhase, SessionMethodRequest, SessionMethodResult, session_methods_server::SessionMethods,
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[tonic::async_trait]
impl SessionMethods for RuntimeService {
    async fn call(
        &self,
        request: Request<SessionMethodRequest>,
    ) -> std::result::Result<Response<SessionMethodResult>, Status> {
        self.check(&request)?;
        let request = request.into_inner();
        if request.identity.as_ref() != Some(&self.identity) {
            return Err(Status::permission_denied("process"));
        }
        let caller = request.caller.as_ref().ok_or(Status::permission_denied("caller"))?;
        let bindings = self.bindings.lock().unwrap();
        let binding = bindings.get(&caller.delivery_operation_id).ok_or(Status::permission_denied("delivery"))?;
        if binding.phase != DeliveryPhase::Arrived
            || binding.delivery.owner_generation != caller.owner_generation
            || binding.delivery.membership_generation != caller.membership_generation
            || binding.delivery.player != caller.player
        {
            return Err(Status::permission_denied("stale caller"));
        }
        let mut requests = self.method_requests.lock().unwrap();
        if let Some(previous) = requests.get(&request.operation_id) {
            if previous != &request {
                return Err(Status::already_exists("changed"));
            }
        } else {
            requests.insert(request.operation_id.clone(), request.clone());
        }
        if self.lost_reply.swap(false, Ordering::AcqRel) {
            return Err(Status::deadline_exceeded("lost method result"));
        }
        Ok(Response::new(SessionMethodResult {
            operation_id: request.operation_id,
            phase: SessionMethodPhase::Completed as i32,
            result_json: "7".into(),
            error: None,
        }))
    }
    async fn cancel(
        &self,
        request: Request<SessionMethodRequest>,
    ) -> std::result::Result<Response<SessionMethodResult>, Status> {
        self.check(&request)?;
        let request = request.into_inner();
        let completed = self.method_requests.lock().unwrap().contains_key(&request.operation_id);
        Ok(Response::new(SessionMethodResult {
            operation_id: request.operation_id,
            phase: if completed { SessionMethodPhase::Completed } else { SessionMethodPhase::Cancelled } as i32,
            result_json: if completed { "7".into() } else { String::new() },
            error: None,
        }))
    }
}

#[tokio::test]
async fn captured_methods_require_live_authority_and_keep_operation_identity_across_retries() {
    let mut fixture = Fixture::new().await;
    fixture.config.contracts.session_methods = Some(method_contract());
    let control = fixture.control();
    let claim = request("method-caller", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(claim.clone()).await.unwrap();
    let identity = assignment.claim.clone().unwrap();
    assert!(control.capture_session(&identity).is_err());
    fixture.runtime.bindings.lock().unwrap().get_mut(&claim.operation_id).unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: Some(identity.clone()) }).await.unwrap();
    let captured = control.capture_session(&identity).unwrap();
    let mut wrong = identity.clone();
    wrong.membership_generation += 1;
    assert!(control.capture_session(&wrong).is_err());
    let timeout = Duration::from_secs(10);
    assert!(control.prepare_session_method(&captured, "hidden", json!({}), timeout).is_err());
    assert!(control.prepare_session_method(&captured, "score", json!({"authority":"admin"}), timeout).is_err());
    let operation = control.prepare_session_method(&captured, "score", json!({}), timeout).unwrap();
    let result = control.call_session_method(&operation, &CancellationToken::new()).await.unwrap();
    assert_eq!(result.phase, SessionMethodPhase::Completed as i32);
    assert_eq!(result.result_json, "7");
    assert_eq!(result, control.call_session_method(&operation, &CancellationToken::new()).await.unwrap());
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 1);
    let lost = control.prepare_session_method(&captured, "score", json!({}), timeout).unwrap();
    fixture.runtime.lost_reply.store(true, Ordering::Release);
    assert_eq!(
        control.call_session_method(&lost, &CancellationToken::new()).await.unwrap().phase,
        SessionMethodPhase::Unknown as i32
    );
    assert_eq!(control.call_session_method(&lost, &CancellationToken::new()).await.unwrap().result_json, "7");
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 2);
    drop(control);
    let control = fixture.control();
    let next = control.prepare_session_method(&captured, "score", json!({}), timeout).unwrap();
    assert_ne!(next.operation_id(), operation.operation_id());
    assert_ne!(next.operation_id(), lost.operation_id());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        control.call_session_method(&next, &cancelled).await.unwrap().phase,
        SessionMethodPhase::Cancelled as i32
    );
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 2);
    control.cancel(claim).await.unwrap();
    assert!(control.capture_session(&identity).is_err());
    assert!(control.prepare_session_method(&captured, "score", json!({}), timeout).is_err());
    fixture.close().await;
}

pub(super) fn method_contract() -> chunk_contract::SessionMethods {
    serde_json::from_value(json!({"version":1,"methods":[{"app":"bridge","session":"default","name":"score","arguments":{"type":"object","fields":{}},"result":{"type":"integer"}}]})).unwrap()
}
