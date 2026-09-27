use super::*;

/// Claims `operation` for a new player on the fake JVM, then closes the fake JVM's stream. Returns the claim's host.
async fn claimed(fixture: &Fixture, control: &Control, operation: &str) -> String {
    control.claim(request(operation, &uuid::Uuid::new_v4().to_string())).await.unwrap();
    fixture.detach().await;
    let state = control.state().unwrap();
    state.sessions[&state.claims[operation].session].host.clone()
}

#[tokio::test]
async fn a_reported_arrival_reaches_the_watching_proxy_without_polling() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    // The stream finds its host by the JVM's runtime ID, which the fake JVM shares across hosts.
    let host = crate::state::HostState::requested("build", "bridge", "local");
    control
        .update(|state| {
            state.hosts.insert(fixture.runtime.identity.runtime_id.clone(), host);
            Ok(())
        })
        .unwrap();
    claimed(&fixture, &control, "arriving").await;
    let (reports, inbound) = tokio::sync::mpsc::channel(4);
    let (sender, mut desired) = tokio::sync::mpsc::channel(1);
    let jvm = control.clone();
    let inbound = tokio_stream::wrappers::ReceiverStream::new(inbound);
    let stream = tokio::spawn(async move {
        jvm.sync("test-runtime-credential".into(), inbound, sender, CancellationToken::new()).await;
    });
    reports.send(Ok(fixture.runtime.report())).await.unwrap();
    let snapshot = desired.recv().await.unwrap().unwrap();
    assert_eq!(snapshot.create.len(), 1);
    assert!(snapshot.finish.is_empty());

    let (updates, mut watched) = tokio::sync::mpsc::channel(1);
    let proxy = control.clone();
    let watch = tokio::spawn(async move { proxy.watch("proxy-1".into(), updates, CancellationToken::new()).await });
    let [reserved] = watched.recv().await.unwrap().unwrap().claims.try_into().unwrap();
    assert_eq!(reserved.phase, ClaimPhase::Reserved as i32);
    fixture.runtime.bindings.lock().unwrap().get_mut("arriving").unwrap().phase = DeliveryPhase::Arrived;
    reports.send(Ok(fixture.runtime.report())).await.unwrap();
    let update = tokio::time::timeout(Duration::from_secs(1), watched.recv()).await.unwrap().unwrap().unwrap();
    let [arrived] = update.claims.try_into().unwrap();
    assert_eq!(arrived.phase, ClaimPhase::Arrived as i32);
    watch.abort();
    stream.abort();
    fixture.close().await;
}

#[tokio::test]
async fn a_reconnecting_jvm_is_repaired_in_one_pass() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    control.claim(request("leaving", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let host = claimed(&fixture, &control, "arriving").await;
    // While its stream is down, one player arrives and the other leaves.
    {
        let mut bindings = fixture.runtime.bindings.lock().unwrap();
        bindings.get_mut("arriving").unwrap().phase = DeliveryPhase::Arrived;
        bindings.get_mut("leaving").unwrap().phase = DeliveryPhase::Closed;
    }
    control.attach(&host, "test-runtime-credential", fixture.runtime.report()).await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims["arriving"].phase == Phase::Arrived);
    assert!(state.claims["leaving"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn stale_generations_and_replaced_processes_cannot_write_back() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let host = claimed(&fixture, &control, "claimed").await;
    let stream = control.attach(&host, "test-runtime-credential", fixture.runtime.report()).await.unwrap();
    let mut stale = fixture.runtime.report();
    let delivery = stale.deliveries[0].delivery.as_mut().unwrap();
    delivery.owner_generation -= 1;
    stale.deliveries[0].phase = DeliveryPhase::Closed as i32;
    control.report(&host, stream, &stale).await.unwrap();
    assert!(control.state().unwrap().claims["claimed"].phase == Phase::Reserved);

    let mut replaced = fixture.runtime.report();
    replaced.identity.as_mut().unwrap().process_id = "replaced".into();
    replaced.deliveries[0].phase = DeliveryPhase::Closed as i32;
    assert!(control.report(&host, stream, &replaced).await.is_err());
    assert!(control.attach(&host, "another-credential", fixture.runtime.report()).await.is_err());
    assert!(control.state().unwrap().claims["claimed"].phase == Phase::Reserved);

    // Once a replacement stream reports an arrival, neither the old stream nor an older phase can undo it.
    let mut attached = fixture.runtime.report();
    attached.deliveries[0].phase = DeliveryPhase::Attached as i32;
    let mut arrived = attached.clone();
    arrived.deliveries[0].phase = DeliveryPhase::Arrived as i32;
    let replacement = control.attach(&host, "test-runtime-credential", arrived).await.unwrap();
    assert!(control.report(&host, stream, &attached).await.is_err());
    control.report(&host, replacement, &attached).await.unwrap();
    assert!(control.state().unwrap().claims["claimed"].phase == Phase::Arrived);
    fixture.close().await;
}

#[tokio::test]
async fn a_jvm_names_a_player_from_reservation_until_its_delivery_closes() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let host = claimed(&fixture, &control, "held").await;
    let state = control.state().unwrap();
    let (session, player) = (state.claims["held"].session.clone(), state.claims["held"].player.clone());
    let scope = |player| control.session_scope(&host, &session, Some(player));
    assert!(state.claims["held"].phase == Phase::Reserved);
    assert!(scope(&player).is_ok());
    // The JVM runs join hooks before core applies its Attached report.
    control
        .update(|state| {
            state.claims.get_mut("held").unwrap().phase = Phase::Activating;
            Ok(())
        })
        .unwrap();
    assert!(scope(&player).is_ok());

    let stream = control.attach(&host, "test-runtime-credential", fixture.runtime.report()).await.unwrap();
    let mut report = fixture.runtime.report();
    for (phase, held) in
        [(DeliveryPhase::Attached, true), (DeliveryPhase::Withdrawing, true), (DeliveryPhase::Closed, false)]
    {
        report.deliveries[0].phase = phase as i32;
        control.report(&host, stream, &report).await.unwrap();
        assert_eq!(scope(&player).is_ok(), held, "{phase:?}");
    }
    assert!(scope("another-player").is_err());
    fixture.close().await;
}
