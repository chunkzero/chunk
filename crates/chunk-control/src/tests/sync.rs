use super::*;
use chunk_proto::sync::v1::{JvmDeliveryStatus, JvmReport};

/// Claims `operation` for a new player on the fake JVM, then closes the fake JVM's stream. Returns the claim's host.
async fn claimed(fixture: &Fixture, control: &Control, operation: &str) -> String {
    control.claim(request(operation, &uuid::Uuid::new_v4().to_string())).await.unwrap();
    fixture.detach().await;
    let state = control.state().unwrap();
    state.sessions[&state.claims[operation].session].host.clone()
}

/// Opens `stream` on `host`'s topic, as the host's JVM.
fn open(control: &Arc<Control>, host: &str, stream: &str) -> crate::jvm::Topic {
    crate::jvm::Topic::open(control, host, stream).unwrap().0
}

/// Everything the fake JVM holds on `host`, as a stream's first report states it.
fn complete(fixture: &Fixture, host: &str) -> JvmReport {
    JvmReport { complete: true, ..fixture.host.report(host) }
}

/// `report` with its only delivery set to `phase`.
fn delivered(mut report: JvmReport, phase: JvmDeliveryPhase) -> JvmReport {
    assert_eq!(report.deliveries.len(), 1);
    report.deliveries[0].phase = phase.into();
    report
}

#[tokio::test]
async fn a_reported_arrival_reaches_the_watching_proxy_without_polling() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    control.claim(request("arriving", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let mut positions = control.subscribe();
    let (mut view, snapshot) = View::open(&control, "proxy-1", None).unwrap();
    let [(_, reserved)] = &snapshot.upserts[..] else { panic!("expected one claim") };
    assert!(reserved.phase == Phase::Reserved);
    fixture.runtime.bindings.lock().unwrap().get_mut("arriving").unwrap().phase = JvmDeliveryPhase::Arrived;
    let update = changed(&control, &mut view, &mut positions).await;
    let [(_, arrived)] = &update.upserts[..] else { panic!("expected one claim") };
    assert!(arrived.phase == Phase::Arrived);
    fixture.close().await;
}

#[tokio::test]
async fn a_reconnecting_jvm_is_repaired_in_one_pass() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    control.claim(request("leaving", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    let host = claimed(&fixture, &control, "arriving").await;
    // While its stream is down, one player arrives and the other leaves.
    {
        let mut bindings = fixture.runtime.bindings.lock().unwrap();
        bindings.get_mut("arriving").unwrap().phase = JvmDeliveryPhase::Arrived;
        bindings.get_mut("leaving").unwrap().phase = JvmDeliveryPhase::Closed;
    }
    let _topic = open(&control, &host, "reconnected");
    control.report_jvm(&host, CREDENTIAL, "reconnected", &complete(&fixture, &host)).unwrap();
    let state = control.state().unwrap();
    assert!(state.claims["arriving"].phase == Phase::Arrived);
    assert!(state.claims["leaving"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn stale_generations_and_replaced_streams_cannot_write_back() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let host = claimed(&fixture, &control, "claimed").await;
    let _older = open(&control, &host, "older");
    control.report_jvm(&host, CREDENTIAL, "older", &complete(&fixture, &host)).unwrap();
    let mut stale = delivered(fixture.host.report(&host), JvmDeliveryPhase::Closed);
    stale.deliveries[0].generation.as_mut().unwrap().revision -= 1;
    control.report_jvm(&host, CREDENTIAL, "older", &stale).unwrap();
    assert!(control.state().unwrap().claims["claimed"].phase == Phase::Reserved);

    // Once a replacement stream reports an arrival, neither the old stream nor an older phase can undo it.
    let _replacement = open(&control, &host, "replacement");
    let arrived = delivered(complete(&fixture, &host), JvmDeliveryPhase::Arrived);
    assert!(control.report_jvm(&host, "another-credential", "replacement", &arrived).is_err());
    control.report_jvm(&host, CREDENTIAL, "replacement", &arrived).unwrap();
    let attached = delivered(fixture.host.report(&host), JvmDeliveryPhase::Attached);
    assert!(matches!(control.report_jvm(&host, CREDENTIAL, "older", &attached), Err(Error::Stopped)));
    control.report_jvm(&host, CREDENTIAL, "replacement", &attached).unwrap();
    assert!(control.state().unwrap().claims["claimed"].phase == Phase::Arrived);
    fixture.close().await;
}

#[tokio::test]
async fn a_jvm_names_a_player_from_reservation_until_its_delivery_closes() {
    let fixture = Fixture::new();
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

    let _topic = open(&control, &host, "held");
    control.report_jvm(&host, CREDENTIAL, "held", &complete(&fixture, &host)).unwrap();
    for (phase, held) in
        [(JvmDeliveryPhase::Attached, true), (JvmDeliveryPhase::Withdrawing, true), (JvmDeliveryPhase::Closed, false)]
    {
        control.report_jvm(&host, CREDENTIAL, "held", &delivered(fixture.host.report(&host), phase)).unwrap();
        assert_eq!(scope(&player).is_ok(), held, "{phase:?}");
    }
    assert!(scope("another-player").is_err());
    fixture.close().await;
}

#[test]
fn a_jvms_inventory_outlives_its_stream_until_a_newer_stream_reports() {
    let links = crate::sync::Links::default();
    let identity = crate::JvmIdentity { host: "host".into(), ..crate::JvmIdentity::default() };
    let report = |operation: &str| JvmReport {
        deliveries: vec![JvmDeliveryStatus {
            operation_id: operation.into(),
            phase: JvmDeliveryPhase::Arrived.into(),
            ..JvmDeliveryStatus::default()
        }],
        ..JvmReport::default()
    };
    let first = links.attach("host", identity.clone(), &report("kept")).unwrap();
    links.detach("host", first);
    // A reconnecting JVM's deliveries stand, but its ended stream no longer reports.
    assert!(links.delivery("host", &identity, "kept").is_some());
    assert!(links.merge("host", first, &identity, &report("late")).is_err());
    let second = links.attach("host", identity.clone(), &report("current")).unwrap();
    assert!(links.delivery("host", &identity, "kept").is_none());
    links.merge("host", second, &identity, &report("later")).unwrap();
    assert!(links.delivery("host", &identity, "later").is_some());
}
