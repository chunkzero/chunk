use super::*;
use chunk_proto::v1::{ProcessRegistration, supervisor_server::Supervisor};

#[tokio::test]
async fn an_unreachable_surviving_jvm_keeps_admission_closed_past_the_deadline_until_it_is_confirmed_stopped() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    control.claim(request("served", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["served"].session].host.clone();
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control().await;
    control.recovery.pass_deadline();
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.admit().await, Err(Error::Busy)));
    fixture.host.terminated.lock().unwrap().insert(host.clone());
    eventually(|| control.state().unwrap().released(&host)).await;
    control.admit().await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn restored_reservations_are_neither_prepared_nor_activated_until_survivors_are_fenced() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let prepared = control.claim(request("prepared", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    fixture.runtime.available.store(false, Ordering::Release);
    let reserved = request("reserved", &uuid::Uuid::new_v4().to_string());
    assert!(control.claim(reserved.clone()).await.is_err());
    assert!(control.state().unwrap().claims["reserved"].assignment.is_none());
    fixture.runtime.available.store(true, Ordering::Release);
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    // The lost tail may have canceled either claim and admitted its player on a JVM that has not re-attached.
    let control = fixture.control().await;
    assert!(matches!(control.claim(reserved.clone()).await, Err(Error::Busy)));
    assert!(matches!(control.activate(ActivateClaim { claim: prepared.claim }).await, Err(Error::Busy)));
    assert!(!fixture.runtime.bindings.lock().unwrap().contains_key("reserved"));
    fixture.close().await;
}

#[tokio::test]
async fn a_surviving_jvm_reattaches_with_its_logged_credential_and_keeps_owned_claims() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let arrived = request("arrived", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(arrived.clone()).await.unwrap();
    fixture.arrive(&control, "arrived").await;
    control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["arrived"].session].host.clone();
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    // The surviving JVM also holds a delivery the log does not know.
    let stray = PlayerDelivery {
        operation_id: "stray".into(),
        owner_generation: assignment.claim.as_ref().unwrap().delivery_generation,
        ..assignment.delivery.clone().unwrap()
    };
    let binding = Binding { delivery: stray, phase: DeliveryPhase::Arrived };
    fixture.runtime.bindings.lock().unwrap().insert("stray".into(), binding);

    let control = fixture.control().await;
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
    fixture.recovered(&control).await;
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, DeliveryPhase::Closed);
    let current = control.inspect(&arrived).unwrap();
    assert_eq!(current.phase, ClaimPhase::Arrived as i32);
    assert_eq!(current.claim, assignment.claim);
    control.claim(newcomer).await.unwrap();
    fixture.close().await;
}

/// Restarts control while its surviving JVM also holds a delivery the log does not own, such as one a restore lost.
/// Returns the restarted control and the JVM's re-registration.
async fn restart_with_stray_delivery(fixture: &Fixture) -> (Arc<Control>, ProcessRegistration) {
    let control = fixture.control().await;
    let assignment = control.claim(request("served", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["served"].session].host.clone();
    drop(control);
    let stray = PlayerDelivery { operation_id: "stray".into(), ..assignment.delivery.unwrap() };
    let binding = Binding { delivery: stray, phase: DeliveryPhase::Prepared };
    fixture.runtime.bindings.lock().unwrap().insert("stray".into(), binding);
    fixture.host.forgotten.store(true, Ordering::Release);
    let registration = ProcessRegistration {
        identity: Some(ProcessIdentity { runtime_id: host, ..fixture.runtime.identity.clone() }),
        control_endpoint: fixture.host.endpoint.clone(),
        player_endpoint: "127.0.0.1:1".into(),
    };
    (fixture.control().await, registration)
}

#[tokio::test]
async fn a_jvm_re_attaching_while_recovery_finds_no_connection_is_still_fenced() {
    let fixture = Fixture::new().await;
    let (control, registration) = restart_with_stray_delivery(&fixture).await;
    let registering = control.clone();
    // The JVM re-attaches after recovery finds no connection and before it checks for an unowned launch.
    *fixture.host.missed.lock().unwrap() = Some(Box::new(move || {
        registering.register("Bearer test-runtime-credential", registration).unwrap();
    }));
    assert!(matches!(control.admit().await, Err(Error::Busy)));
    fixture.recovered(&control).await;
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, DeliveryPhase::Closed);
    fixture.close().await;
}

#[tokio::test]
async fn a_jvm_paused_between_adoption_and_its_recovery_stamp_is_still_fenced() {
    let fixture = Fixture::new().await;
    let (control, registration) = restart_with_stray_delivery(&fixture).await;
    let (adopted, published) = std::sync::mpsc::channel();
    // Registration pauses once its adopted process is visible, before recovery learns of the re-attachment.
    *fixture.host.adopted.lock().unwrap() = Some(Box::new(move || {
        adopted.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
    }));
    let paused = Arc::new(Mutex::new(None));
    let (thread, registering) = (paused.clone(), control.clone());
    // Recovery finds no connection, then checks for an unowned launch while registration is paused.
    *fixture.host.missed.lock().unwrap() = Some(Box::new(move || {
        let register = move || registering.register("Bearer test-runtime-credential", registration).unwrap();
        *thread.lock().unwrap() = Some(std::thread::spawn(register));
        published.recv().unwrap();
    }));
    assert!(matches!(control.admit().await, Err(Error::Busy)));
    paused.lock().unwrap().take().unwrap().join().unwrap();
    fixture.recovered(&control).await;
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, DeliveryPhase::Closed);
    fixture.close().await;
}

#[test]
fn recovery_fences_its_hosts_unassigned_reservations_but_never_another_hosts() {
    let mut state = crate::state::State::default();
    let generation = crate::state::Generation::new(1, 5).unwrap();
    for host in ["a", "b"] {
        let session = crate::state::SessionState {
            empty_since_ms: None,
            finish_requested: false,
            finished: false,
            host: host.into(),
            session_type: "bridge/default".into(),
            demand_key: "lobby".into(),
            capacity: 8,
            configuration: serde_json::json!({}),
            retired: false,
        };
        state.sessions.insert(host.into(), session);
        let claim = crate::state::Claim {
            request: host.as_bytes().to_vec(),
            player: host.into(),
            proxy: "proxy-1".into(),
            membership: generation,
            generation,
            session: host.into(),
            phase: Phase::Reserved,
            assignment: None,
            activated: false,
            created_at_ms: 0,
            released_at_ms: None,
            roster: None,
        };
        state.claims.insert(host.into(), claim);
    }
    // Recovering host A's JVM reports its own reservation prepared and B's, by operation and generation, arrived.
    let binding = |operation: &str, phase: DeliveryPhase| DeliveryInventory {
        delivery: Some(PlayerDelivery {
            operation_id: operation.into(),
            owner_generation: generation.wire(),
            membership_generation: generation.wire(),
            ..PlayerDelivery::default()
        }),
        phase: phase.into(),
    };
    let report = ProcessReport {
        deliveries: vec![binding("a", DeliveryPhase::Prepared), binding("b", DeliveryPhase::Arrived)],
        ..ProcessReport::default()
    };
    crate::recovery::retire_unowned(&mut state, "a", &report).unwrap();
    let (a, b) = (&state.claims["a"], &state.claims["b"]);
    assert!(a.phase == Phase::Released && a.request.is_empty());
    assert!(b.phase == Phase::Reserved && b.request == b"b");
    assert!(!crate::jvm::owned(&state, "a", report.deliveries[1].delivery.as_ref().unwrap()));
}
