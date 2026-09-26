use super::*;
use chunk_proto::v1::{ProcessRegistration, supervisor_server::Supervisor};

#[tokio::test]
async fn an_unreachable_surviving_jvm_keeps_admission_closed_past_the_deadline_until_it_is_confirmed_stopped() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    control.claim(request("served", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["served"].session].host.clone();
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control();
    control.recovery.pass_deadline();
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.admit().await, Err(Error::Busy)));
    fixture.host.terminated.lock().unwrap().insert(host);
    control.admit().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn restored_reservations_are_neither_prepared_nor_activated_until_survivors_are_fenced() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let prepared = control.claim(request("prepared", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    fixture.runtime.available.store(false, Ordering::Release);
    let reserved = request("reserved", &uuid::Uuid::new_v4().to_string());
    assert!(control.claim(reserved.clone()).await.is_err());
    assert!(control.state().unwrap().claims["reserved"].assignment.is_none());
    fixture.runtime.available.store(true, Ordering::Release);
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    // The lost tail may have canceled either claim and admitted its player on a JVM that has not re-attached.
    let control = fixture.control();
    assert!(matches!(control.claim(reserved.clone()).await, Err(Error::Busy)));
    assert!(matches!(control.activate(ActivateClaim { claim: prepared.claim }).await, Err(Error::Busy)));
    assert!(!fixture.runtime.bindings.lock().unwrap().contains_key("reserved"));
    fixture.close().await;
}

#[tokio::test]
async fn a_surviving_jvm_reattaches_with_its_logged_credential_and_keeps_owned_claims() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let arrived = request("arrived", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(arrived.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("arrived").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["arrived"].session].host.clone();
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control();
    assert!(control.inspect(arrived.clone()).await.is_err());
    // Until the surviving JVM re-attaches and is fenced, no new login is admitted.
    let newcomer = request("newcomer", &uuid::Uuid::new_v4().to_string());
    assert!(matches!(control.claim(newcomer.clone()).await, Err(Error::Busy)));
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.claim(newcomer.clone()).await, Err(Error::Busy)));
    let service = crate::Service::new(control.clone(), "control-group-credential-with-32-characters".into()).unwrap();
    let register = |token: &str, process: &str| {
        let identity = ProcessIdentity {
            runtime_id: host.clone(),
            process_id: process.into(),
            ..fixture.runtime.identity.clone()
        };
        let mut request = Request::new(ProcessRegistration {
            identity: Some(identity),
            control_endpoint: fixture.host.endpoint.clone(),
            player_endpoint: "127.0.0.1:1".into(),
        });
        request.metadata_mut().insert("authorization", token.parse().unwrap());
        request
    };
    for (token, process) in [("Bearer another-credential", "jvm"), ("Bearer test-runtime-credential", "other-jvm")] {
        assert!(service.register_process(register(token, process)).await.is_err());
        assert!(fixture.host.forgotten.load(Ordering::Acquire));
    }
    service.register_process(register("Bearer test-runtime-credential", "jvm")).await.unwrap();
    assert!(!fixture.host.forgotten.load(Ordering::Acquire));

    // The log owns the arrived delivery; a delivery it does not know is withdrawn.
    let stray = PlayerDelivery {
        operation_id: "stray".into(),
        owner_generation: assignment.claim.as_ref().unwrap().delivery_generation,
        ..assignment.delivery.clone().unwrap()
    };
    let binding = Binding { delivery: stray, phase: DeliveryPhase::Arrived };
    fixture.runtime.bindings.lock().unwrap().insert("stray".into(), binding);
    control.reconcile_all().await.unwrap();
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, DeliveryPhase::Closed);
    let current = control.inspect(arrived).await.unwrap();
    assert_eq!(current.phase, ClaimPhase::Arrived as i32);
    assert_eq!(current.claim, assignment.claim);
    control.claim(newcomer).await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn a_jvm_re_attaching_while_recovery_finds_no_connection_is_still_fenced() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let assignment = control.claim(request("served", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["served"].session].host.clone();
    drop(control);
    // The JVM also holds a delivery the log does not own, such as one a restore lost.
    let stray = PlayerDelivery { operation_id: "stray".into(), ..assignment.delivery.unwrap() };
    let binding = Binding { delivery: stray, phase: DeliveryPhase::Prepared };
    fixture.runtime.bindings.lock().unwrap().insert("stray".into(), binding);
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control();
    let registering = control.clone();
    let registration = ProcessRegistration {
        identity: Some(ProcessIdentity { runtime_id: host, ..fixture.runtime.identity.clone() }),
        control_endpoint: fixture.host.endpoint.clone(),
        player_endpoint: "127.0.0.1:1".into(),
    };
    // The JVM re-attaches after recovery finds no connection and before it checks for an unowned launch.
    *fixture.host.missed.lock().unwrap() = Some(Box::new(move || {
        registering.register("Bearer test-runtime-credential", registration).unwrap();
    }));
    assert!(matches!(control.admit().await, Err(Error::Busy)));
    control.admit().await.unwrap();
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, DeliveryPhase::Closed);
    fixture.close().await;
}
