use super::*;
use crate::Service;
use chunk_proto::v1::{
    MovePlayerRequest, PrepareSessionMethodRequest, PreparedMethodRequest, SessionMethodPhase,
    local_control_client::LocalControlClient,
    local_control_server::{LocalControl, LocalControlServer},
};

const TOKEN: &str = "control-group-credential-with-32-characters";
fn auth<T>(value: T, token: &str) -> Request<T> {
    let mut request = Request::new(value);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

#[tokio::test]
async fn prepared_methods_authenticate_pinned_handles_and_start_only_once() {
    let mut fixture = Fixture::new().await;
    fixture.config.contracts.session_methods = Some(super::session_methods::method_contract());
    let control = fixture.control();
    let source = request("method-source", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(source.clone()).await.unwrap();
    let prepared_request = PrepareSessionMethodRequest {
        claim: assignment.claim.clone(),
        app_id: "bridge".into(),
        session: "default".into(),
        method: "score".into(),
        arguments_json: "{}".into(),
        timeout_ms: 30_000,
    };
    let service = Service::new(control.clone(), TOKEN.into()).unwrap();
    let (mut client, stop, server) = serve_methods(service.clone()).await;
    reject_untrusted_credentials(&mut client, &prepared_request).await;
    assert!(client.prepare_session_method(auth(prepared_request.clone(), TOKEN)).await.is_err()); // Not ARRIVED.
    fixture.runtime.bindings.lock().unwrap().get_mut(&source.operation_id).unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.unwrap();
    reject_invalid_declarations(&mut client, &prepared_request).await;
    let handle = client.prepare_session_method(auth(prepared_request.clone(), TOKEN)).await.unwrap().into_inner();
    let operation = PreparedMethodRequest { operation_id: handle.operation_id.clone() };
    assert!(fixture.runtime.method_requests.lock().unwrap().is_empty());
    assert_eq!(
        client.poll_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap().into_inner().phase,
        SessionMethodPhase::Accepted as i32
    );
    fixture.runtime.lost_reply.store(true, Ordering::Release);
    assert_eq!(
        client.start_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap().into_inner().phase,
        SessionMethodPhase::Accepted as i32
    );
    client.start_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let result = client.poll_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap().into_inner();
            if result.phase == SessionMethodPhase::Completed as i32 {
                break result;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(result.result_json, "7");
    assert_eq!(client.start_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap().into_inner(), result);
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 1);
    let restarted = Service::new(control.clone(), TOKEN.into()).unwrap();
    assert_eq!(
        restarted.start_prepared_method(auth(operation, TOKEN)).await.unwrap().into_inner().phase,
        SessionMethodPhase::Unknown as i32
    );

    let handle = client.prepare_session_method(auth(prepared_request.clone(), TOKEN)).await.unwrap().into_inner();
    let operation = PreparedMethodRequest { operation_id: handle.operation_id };
    assert_eq!(
        client.cancel_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap().into_inner().phase,
        SessionMethodPhase::Cancelled as i32
    );
    assert_eq!(
        client.start_prepared_method(auth(operation, TOKEN)).await.unwrap().into_inner().phase,
        SessionMethodPhase::Cancelled as i32
    );
    let expired = client
        .prepare_session_method(auth(PrepareSessionMethodRequest { timeout_ms: 1, ..prepared_request.clone() }, TOKEN))
        .await
        .unwrap()
        .into_inner();
    tokio::time::sleep(Duration::from_millis(5)).await;
    assert_eq!(
        client
            .start_prepared_method(auth(PreparedMethodRequest { operation_id: expired.operation_id }, TOKEN))
            .await
            .unwrap()
            .into_inner()
            .phase,
        SessionMethodPhase::Unknown as i32
    );

    let stale = client.prepare_session_method(auth(prepared_request.clone(), TOKEN)).await.unwrap().into_inner();
    control.cancel(source.clone()).await.unwrap();
    assert_eq!(
        client
            .start_prepared_method(auth(PreparedMethodRequest { operation_id: stale.operation_id }, TOKEN))
            .await
            .unwrap()
            .into_inner()
            .phase,
        SessionMethodPhase::Cancelled as i32
    );
    assert!(client.prepare_session_method(auth(prepared_request, TOKEN)).await.is_err());
    assert_eq!(fixture.runtime.method_requests.lock().unwrap().len(), 1);
    drop(client);
    service.close_methods();
    service.operations().close();
    service.operations().wait().await;
    let _ = stop.send(());
    server.await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn prepared_methods_bound_pending_handles_and_cancel_before_shutdown_wait() {
    let mut fixture = Fixture::new().await;
    fixture.config.contracts.session_methods = Some(super::session_methods::method_contract());
    let control = fixture.control();
    let source = request("method-capacity", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut(&source.operation_id).unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.unwrap();
    let service = Service::new(control, TOKEN.into()).unwrap();
    let request = PrepareSessionMethodRequest {
        claim: assignment.claim,
        app_id: "bridge".into(),
        session: "default".into(),
        method: "score".into(),
        arguments_json: "{}".into(),
        timeout_ms: 30_000,
    };
    let mut handles = Vec::new();
    for _ in 0..128 {
        handles.push(service.prepare_session_method(auth(request.clone(), TOKEN)).await.unwrap().into_inner());
    }
    assert_eq!(
        service.prepare_session_method(auth(request.clone(), TOKEN)).await.unwrap_err().code(),
        tonic::Code::ResourceExhausted
    );
    let operation = PreparedMethodRequest { operation_id: handles[0].operation_id.clone() };
    fixture.runtime.available.store(false, Ordering::Release);
    service.start_prepared_method(auth(operation.clone(), TOKEN)).await.unwrap();
    service.close_methods();
    service.operations().close();
    tokio::time::timeout(Duration::from_secs(3), service.operations().wait()).await.unwrap();
    assert_eq!(
        service.poll_prepared_method(auth(operation, TOKEN)).await.unwrap().into_inner().phase,
        SessionMethodPhase::Unknown as i32
    );
    assert_eq!(
        service
            .start_prepared_method(auth(PreparedMethodRequest { operation_id: handles[1].operation_id.clone() }, TOKEN))
            .await
            .unwrap()
            .into_inner()
            .phase,
        SessionMethodPhase::Cancelled as i32
    );
    assert!(service.prepare_session_method(auth(request, TOKEN)).await.is_err());
    assert!(fixture.runtime.method_requests.lock().unwrap().is_empty());
    fixture.close().await;
}

#[tokio::test]
async fn captured_moves_reject_replaced_connections_even_for_existing_operations() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("move-source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut(&source.operation_id).unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: first.claim.clone() }).await.unwrap();
    let expected = MovePlayerRequest {
        operation_id: "captured-move".into(),
        player_id: uuid.clone(),
        demand: source.demand.clone(),
        expected_source: first.claim,
        expected_connection_id: source.connection_id.clone(),
    };
    for invalid in [
        MovePlayerRequest { expected_source: None, ..expected.clone() },
        MovePlayerRequest { expected_connection_id: String::new(), ..expected.clone() },
        MovePlayerRequest { expected_connection_id: "replacement".into(), ..expected.clone() },
    ] {
        assert!(control.move_player(invalid).is_err());
    }
    let queued = control.move_player(expected.clone()).unwrap();
    assert_eq!(control.move_player(expected.clone()).unwrap(), queued);
    control.cancel(queued).await.unwrap();
    control.cancel(source).await.unwrap();
    let replacement = request("replacement", &uuid);
    let next = control.claim(replacement.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut(&replacement.operation_id).unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: next.claim.clone() }).await.unwrap();
    assert!(control.move_player(expected.clone()).is_err());
    assert!(
        control
            .move_player(MovePlayerRequest {
                expected_source: next.claim,
                expected_connection_id: replacement.connection_id.clone(),
                ..expected.clone()
            })
            .is_err()
    );
    assert!(control.move_player(MovePlayerRequest { operation_id: "new-stale-move".into(), ..expected }).is_err());
    assert!(pending_move(&control, &replacement).await.is_none());
    fixture.close().await;
}

#[tokio::test]
async fn pinned_method_configuration_validates_declarations_and_keeps_empty_compatibility() {
    let mut fixture = Fixture::new().await;
    let serialized = serde_json::to_value(&fixture.config).unwrap();
    assert!(serialized.get("session_methods").is_none());
    let decoded: Config = serde_json::from_value(serialized).unwrap();
    assert!(decoded.contracts.session_methods.is_none());
    let mut methods = super::session_methods::method_contract();
    fixture.config.contracts.session_methods = Some(methods.clone());
    assert!(fixture.config.validate().is_ok());
    methods.methods[0].session = "missing".into();
    fixture.config.contracts.session_methods = Some(methods);
    assert!(fixture.config.validate().is_err());
    fixture.close().await;
}

async fn reject_untrusted_credentials(
    client: &mut LocalControlClient<tonic::transport::Channel>,
    prepared_request: &PrepareSessionMethodRequest,
) {
    for token in ["", "application-backend-token", "test-runtime-credential"] {
        assert_eq!(
            client.prepare_session_method(auth(prepared_request.clone(), token)).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        let handle = PreparedMethodRequest { operation_id: "jvm/1".into() };
        assert_eq!(
            client.start_prepared_method(auth(handle.clone(), token)).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            client.poll_prepared_method(auth(handle.clone(), token)).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
        assert_eq!(
            client.cancel_prepared_method(auth(handle, token)).await.unwrap_err().code(),
            tonic::Code::Unauthenticated
        );
    }
}

async fn reject_invalid_declarations(
    client: &mut LocalControlClient<tonic::transport::Channel>,
    prepared_request: &PrepareSessionMethodRequest,
) {
    let mut stale = prepared_request.clone();
    stale.claim.as_mut().unwrap().delivery_generation += 1;
    for invalid in [
        stale,
        PrepareSessionMethodRequest { app_id: "foreign".into(), ..prepared_request.clone() },
        PrepareSessionMethodRequest { session: "other".into(), ..prepared_request.clone() },
        PrepareSessionMethodRequest { method: "hidden".into(), ..prepared_request.clone() },
        PrepareSessionMethodRequest { arguments_json: "{\"authority\":\"admin\"}".into(), ..prepared_request.clone() },
        PrepareSessionMethodRequest { timeout_ms: 30_001, ..prepared_request.clone() },
    ] {
        assert!(client.prepare_session_method(auth(invalid, TOKEN)).await.is_err());
    }
}

async fn serve_methods(
    service: Service,
) -> (LocalControlClient<tonic::transport::Channel>, oneshot::Sender<()>, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (stop, stopped) = oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(LocalControlServer::new(service).max_decoding_message_size(65_536))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            })
            .await
            .unwrap();
    });
    let client = LocalControlClient::connect(endpoint).await.unwrap();
    (client, stop, server)
}
