use super::*;
use crate::DrainPolicy;

/// The fixture's release as the next deployment version.
fn next(fixture: &Fixture, name: &str) -> Release {
    let mut release = fixture.release.clone();
    release.deployment.deployment = name.into();
    release
}

/// The release of the host serving `operation`'s claim.
fn release_of(control: &Control, operation: &str) -> String {
    let state = control.state().unwrap();
    state.hosts[&state.sessions[&state.claims[operation].session].host].release.clone()
}

/// Logs `player` in under `operation`, routed with `deployment`, and waits until the player arrived.
async fn arrived(
    fixture: &Fixture,
    control: &Control,
    operation: &str,
    player: &str,
    deployment: &str,
) -> ClaimRequest {
    let request = ClaimRequest { deployment: deployment.into(), ..request(operation, player) };
    let assignment = control.claim(request.clone()).await.unwrap();
    fixture.arrive(control, operation).await;
    control.activate(assignment.claim.unwrap()).await.unwrap();
    request
}

/// Releases `claim`, as a disconnect does, long enough ago that its release no longer waits for players on their way.
async fn disconnect(control: &Control, claim: ClaimRequest) {
    let operation = claim.operation_id.clone();
    control.cancel(claim).await.unwrap();
    control
        .update(|state| {
            state.claims.get_mut(&operation).unwrap().released_at_ms = Some(crate::now_ms() - 60_000);
            Ok(())
        })
        .unwrap();
}

#[tokio::test]
async fn a_draining_release_retires_once_its_sessions_have_no_players() {
    let mut fixture = Fixture::new();
    fixture.release.apps.get_mut("bridge").unwrap().sessions.get_mut("default").unwrap().reconnect = false;
    let control = fixture.control().await;
    let player = uuid::Uuid::new_v4().to_string();
    let first = arrived(&fixture, &control, "first", &player, "").await;
    let evacuation = crate::moves::evacuation("first", &control.state().unwrap().claims["first"]).unwrap();
    control.activate_release(next(&fixture, "next")).unwrap();
    assert!(!control.drain_release("build", DrainPolicy::default()).unwrap());
    control.progress_releases().unwrap();
    assert!(!control.state().unwrap().releases["build"].retired);
    disconnect(&control, first).await;
    // The session type takes no reconnects, so the player logs in to the current release.
    arrived(&fixture, &control, "again", &player, "next").await;
    assert_eq!(release_of(&control, "again"), "next");
    // An evacuation captured before the player moved on does not move their new claim.
    assert!(matches!(control.move_player(evacuation), Err(Error::Refused(_))));
    assert!(control.state().unwrap().moves.is_empty());
    control.progress_releases().unwrap();
    assert!(control.state().unwrap().releases["build"].retired);
    eventually(|| control.drain_release("build", DrainPolicy::default()).unwrap()).await;
    fixture.close().await;
}

#[tokio::test]
async fn a_draining_release_stops_at_its_deadline_with_players_remaining() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first", &uuid::Uuid::new_v4().to_string(), "").await;
    control.activate_release(next(&fixture, "next")).unwrap();
    let policy = DrainPolicy { max_age: None, deadline: Some(Duration::ZERO) };
    assert!(!control.drain_release("build", policy).unwrap());
    control.progress_releases().unwrap();
    let host = control.state().unwrap().sessions[&control.state().unwrap().claims["first"].session].host.clone();
    eventually(|| control.drain_release("build", policy).unwrap()).await;
    assert!(fixture.host.stopped(&host));
    fixture.close().await;
}

#[tokio::test]
async fn shortened_limits_count_from_when_the_release_started_draining() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    control.activate_release(next(&fixture, "next")).unwrap();
    let hour = Duration::from_secs(3600);
    control.drain_release("build", DrainPolicy { max_age: Some(3 * hour), deadline: Some(4 * hour) }).unwrap();
    let since = crate::now_ms() - 2 * 3_600_000;
    control
        .update(|state| {
            state.releases.get_mut("build").unwrap().drain.as_mut().unwrap().since = since;
            Ok(())
        })
        .unwrap();
    control.drain_release("build", DrainPolicy { max_age: Some(hour), deadline: Some(2 * hour) }).unwrap();
    let state = control.state().unwrap();
    let drain = state.releases["build"].drain.as_ref().unwrap();
    assert_eq!((drain.reconnects_until, drain.stops_at), (Some(since + 3_600_000), Some(since + 7_200_000)));
    fixture.close().await;
}

#[tokio::test]
async fn a_release_deadline_passes_while_withdrawals_are_stuck() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first", &uuid::Uuid::new_v4().to_string(), "").await;
    control.claim(request("stuck", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    control
        .update(|state| {
            state.claims.get_mut("stuck").unwrap().created_at_ms = 0;
            Ok(())
        })
        .unwrap();
    control.activate_release(next(&fixture, "next")).unwrap();
    control.drain_release("build", DrainPolicy { max_age: None, deadline: Some(Duration::ZERO) }).unwrap();
    // Reconciliation's withdrawal of the expired reservation waits on this lock.
    let lock = control.operation("stuck").unwrap();
    let held = lock.lock().await;
    let reconciling = tokio::spawn({
        let control = control.clone();
        async move { control.reconcile_all().await }
    });
    eventually(|| control.state().unwrap().releases["build"].retired).await;
    assert!(!reconciling.is_finished());
    drop(held);
    reconciling.await.unwrap().unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn a_player_reconnects_to_the_session_they_left_on_a_draining_release() {
    let mut fixture = Fixture::new();
    fixture.release.contracts.destinations = Some(
        serde_json::from_value(serde_json::json!({
            "version":1,"entries":{"shared/destinations/lobby":{
                "destination":{"key":"lobby","session_type":"bridge/default","machine_profile":"local"},
                "overflow":"reject","empty_timeout_seconds":1
            }}
        }))
        .unwrap(),
    );
    let control = fixture.control().await;
    let player = uuid::Uuid::new_v4().to_string();
    let first = arrived(&fixture, &control, "first", &player, "").await;
    control.activate_release(next(&fixture, "next")).unwrap();
    control.drain_release("build", DrainPolicy::default()).unwrap();
    let left = control.state().unwrap().claims["first"].session.clone();
    disconnect(&control, first).await;
    // A failed reconnect attempt, which never activated, is not the claim the player left.
    let attempt = ClaimRequest { deployment: "next".into(), ..request("attempt", &player) };
    control.claim(attempt.clone()).await.unwrap();
    disconnect(&control, attempt).await;
    // The release waits for the player within the reconnect grace, past the session's empty timeout.
    control
        .update(|state| {
            state.sessions.get_mut(&left).unwrap().empty_since_ms = Some(crate::now_ms() - 90_000);
            Ok(())
        })
        .unwrap();
    control.reconcile_all().await.unwrap();
    assert!(!control.state().unwrap().sessions[&left].retired);
    assert!(!control.state().unwrap().releases["build"].retired);
    // Routed through the current release, the login returns to the session it left, under a new claim.
    arrived(&fixture, &control, "again", &player, "next").await;
    let state = control.state().unwrap();
    assert_eq!(state.claims["again"].session, left);
    assert!(state.claims["again"].generation > state.claims["first"].generation);
    arrived(&fixture, &control, "other", &uuid::Uuid::new_v4().to_string(), "next").await;
    assert_eq!(release_of(&control, "other"), "next");
    fixture.close().await;
}

#[tokio::test]
async fn the_longest_draining_release_retires_to_make_room() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    for name in ["second", "third"] {
        control.activate_release(next(&fixture, name)).unwrap();
    }
    for name in ["build", "second"] {
        control.drain_release(name, DrainPolicy::default()).unwrap();
    }
    let draining = ["second".to_owned(), "build".to_owned(), "third".to_owned()];
    assert_eq!(control.retire_longest_draining(&draining).unwrap().as_deref(), Some("build"));
    // Nothing more retires while one is stopping for room.
    assert_eq!(control.retire_longest_draining(&draining).unwrap().as_deref(), Some("build"));
    assert!(!control.state().unwrap().releases["second"].retired);
    fixture.close().await;
}
