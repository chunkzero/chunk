use super::*;
use chunk_proto::sync::v1::{JvmDeliveryStatus, JvmRegistration, JvmReport};

#[tokio::test]
async fn an_unreachable_surviving_jvm_keeps_admission_closed_past_the_deadline_until_it_is_confirmed_stopped() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    control.claim(request("served", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["served"].session].host.clone();
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control().await;
    control.recovery.pass_deadline();
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.admit(), Err(Error::Busy)));
    fixture.host.terminated.lock().unwrap().insert(host.clone());
    eventually(|| control.state().unwrap().released(&host)).await;
    control.admit().unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn restored_reservations_are_neither_prepared_nor_activated_until_survivors_are_fenced() {
    let fixture = Fixture::new();
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
    assert!(matches!(control.activate(prepared.claim.unwrap()).await, Err(Error::Busy)));
    assert!(!fixture.runtime.bindings.lock().unwrap().contains_key("reserved"));
    fixture.close().await;
}

#[tokio::test]
async fn a_surviving_jvm_reattaches_with_its_logged_credential_and_keeps_owned_claims() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let arrived = request("arrived", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(arrived.clone()).await.unwrap();
    fixture.arrive(&control, "arrived").await;
    control.activate(assignment.claim.clone().unwrap()).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["arrived"].session].host.clone();
    drop(control);
    fixture.host.forgotten.store(true, Ordering::Release);

    // The surviving JVM also holds a delivery the log does not know.
    let stray = fixture.runtime.bindings.lock().unwrap()["arrived"].clone();
    fixture.runtime.bindings.lock().unwrap().insert("stray".into(), stray);

    let control = fixture.control().await;
    // Until the surviving JVM re-attaches and is fenced, no new login is admitted.
    let newcomer = request("newcomer", &uuid::Uuid::new_v4().to_string());
    assert!(matches!(control.claim(newcomer.clone()).await, Err(Error::Busy)));
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.claim(newcomer.clone()).await, Err(Error::Busy)));
    let registration =
        |process: &str| JvmRegistration { process_id: process.into(), ..fixture.host.registration(&host) };
    for (credential, process) in [("another-credential", "jvm"), (CREDENTIAL, "other-jvm")] {
        assert!(control.register_jvm(&host, credential, registration(process)).is_err());
        assert!(fixture.host.forgotten.load(Ordering::Acquire));
    }
    control.register_jvm(&host, CREDENTIAL, registration("jvm")).unwrap();
    assert!(!fixture.host.forgotten.load(Ordering::Acquire));

    // The log owns the arrived delivery; the JVM closes a delivery the log does not know.
    fixture.recovered(&control).await;
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, JvmDeliveryPhase::Closed);
    let current = control.inspect(&arrived).unwrap();
    assert_eq!(current.phase, ClaimPhase::Arrived as i32);
    assert_eq!(current.claim, assignment.claim);
    control.claim(newcomer).await.unwrap();
    fixture.close().await;
}

/// Restarts control while its surviving JVM also holds a delivery the log does not own, such as one a restore lost.
/// Returns the restarted control and the host whose JVM survived.
async fn restart_with_stray_delivery(fixture: &Fixture) -> (Arc<Control>, String) {
    let control = fixture.control().await;
    control.claim(request("served", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["served"].session].host.clone();
    drop(control);
    let stray = fixture.runtime.bindings.lock().unwrap()["served"].clone();
    fixture.runtime.bindings.lock().unwrap().insert("stray".into(), stray);
    fixture.host.forgotten.store(true, Ordering::Release);
    (fixture.control().await, host)
}

#[tokio::test]
async fn a_jvm_re_attaching_while_recovery_finds_no_connection_is_still_fenced() {
    let fixture = Fixture::new();
    let (control, host) = restart_with_stray_delivery(&fixture).await;
    let (registering, registration) = (control.clone(), fixture.host.registration(&host));
    // The JVM re-attaches after recovery finds no connection and before it checks for an unowned launch.
    *fixture.host.missed.lock().unwrap() = Some(Box::new(move || {
        registering.register_jvm(&host, CREDENTIAL, registration).unwrap();
    }));
    assert!(matches!(control.admit(), Err(Error::Busy)));
    fixture.recovered(&control).await;
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, JvmDeliveryPhase::Closed);
    fixture.close().await;
}

#[tokio::test]
async fn a_jvm_paused_between_adoption_and_its_recovery_stamp_is_still_fenced() {
    let fixture = Fixture::new();
    let (control, host) = restart_with_stray_delivery(&fixture).await;
    let registration = fixture.host.registration(&host);
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
        let register = move || registering.register_jvm(&host, CREDENTIAL, registration).unwrap();
        *thread.lock().unwrap() = Some(std::thread::spawn(register));
        published.recv().unwrap();
    }));
    assert!(matches!(control.admit(), Err(Error::Busy)));
    paused.lock().unwrap().take().unwrap().join().unwrap();
    fixture.recovered(&control).await;
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["stray"].phase, JvmDeliveryPhase::Closed);
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
    let binding = |operation: &str, phase: JvmDeliveryPhase| JvmDeliveryStatus {
        operation_id: operation.into(),
        generation: crate::gateway::position(generation),
        phase: phase.into(),
        capability: Vec::new(),
    };
    let report = JvmReport {
        deliveries: vec![binding("a", JvmDeliveryPhase::Prepared), binding("b", JvmDeliveryPhase::Arrived)],
        ..JvmReport::default()
    };
    crate::recovery::retire_unowned(&mut state, "a", &report).unwrap();
    let (a, b) = (&state.claims["a"], &state.claims["b"]);
    assert!(a.phase == Phase::Released && a.request.is_empty());
    assert!(b.phase == Phase::Reserved && b.request == b"b");
    assert!(!crate::jvm::owned(&state, "a", &report.deliveries[1]));
}
