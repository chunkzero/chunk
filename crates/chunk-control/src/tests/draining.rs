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
    control.activate_release(next(&fixture, "next"), crate::DrainPolicy::default()).unwrap();
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
    control.activate_release(next(&fixture, "next"), crate::DrainPolicy::default()).unwrap();
    let policy = DrainPolicy { max_age: None, deadline: Some(Duration::ZERO) };
    assert!(!control.drain_release("build", policy).unwrap());
    control.progress_releases().unwrap();
    let host = control.state().unwrap().sessions[&control.state().unwrap().claims["first"].session].host.clone();
    eventually(|| control.drain_release("build", policy).unwrap()).await;
    assert!(fixture.host.stopped(&host));
    fixture.close().await;
}

#[tokio::test]
async fn shortening_the_drain_settings_makes_a_draining_release_due() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first", &uuid::Uuid::new_v4().to_string(), "").await;
    let hours = |hours: u64| Some(Duration::from_secs(hours * 3600));
    control.activate_release(next(&fixture, "next"), DrainPolicy { max_age: hours(1), deadline: hours(4) }).unwrap();
    assert!(control.due_releases().unwrap().is_empty());
    control.set_drain_policy(DrainPolicy { max_age: None, deadline: Some(Duration::ZERO) }).unwrap();
    assert_eq!(control.due_releases().unwrap(), ["build"]);
    fixture.close().await;
}

#[tokio::test]
async fn shortened_limits_count_from_when_the_release_started_draining() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    control.activate_release(next(&fixture, "next"), crate::DrainPolicy::default()).unwrap();
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
    // Settings only shorten each limit on its own: the longer maximum age changes nothing, the shorter deadline applies.
    control.set_drain_policy(DrainPolicy { max_age: Some(6 * hour), deadline: Some(hour) }).unwrap();
    let state = control.state().unwrap();
    let drain = state.releases["build"].drain.as_ref().unwrap();
    assert_eq!((drain.reconnects_until, drain.stops_at), (Some(since + 3_600_000), Some(since + 3_600_000)));
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
            let stuck = state.claims.get_mut("stuck").unwrap();
            (stuck.created_at_ms, stuck.assigned_at_ms) = (0, Some(0));
            Ok(())
        })
        .unwrap();
    control.activate_release(next(&fixture, "next"), crate::DrainPolicy::default()).unwrap();
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
    control.activate_release(next(&fixture, "next"), crate::DrainPolicy::default()).unwrap();
    let left = control.state().unwrap().claims["first"].session.clone();
    disconnect(&control, first).await;
    // A failed reconnect attempt, which never activated, is not the claim the player left.
    let attempt = ClaimRequest { deployment: "next".into(), ..request("attempt", &player) };
    control.claim(attempt.clone()).await.unwrap();
    // Admitted to the lobby, the login stays there though the player could return to the session they left.
    assert_ne!(control.state().unwrap().claims["attempt"].session, left);
    disconnect(&control, attempt).await;
    let wrong = ClaimRequest { reconnect_session: "elsewhere".into(), ..request("wrong", &player) };
    assert!(matches!(control.claim(wrong).await, Err(Error::Unresolved(crate::ROUTE_AGAIN))));
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
    // A login the gateway admitted to that session returns to it, under a new claim.
    let again =
        ClaimRequest { reconnect_session: left.clone(), deployment: "build".into(), ..request("again", &player) };
    let assignment = control.claim(again).await.unwrap();
    fixture.arrive(&control, "again").await;
    control.activate(assignment.claim.unwrap()).await.unwrap();
    let state = control.state().unwrap();
    assert_eq!(state.claims["again"].session, left);
    assert!(state.claims["again"].generation > state.claims["first"].generation);
    arrived(&fixture, &control, "other", &uuid::Uuid::new_v4().to_string(), "next").await;
    assert_eq!(release_of(&control, "other"), "next");
    fixture.close().await;
}

#[tokio::test]
async fn activation_starts_every_other_drain_and_a_rollback_ends_it() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let policy = DrainPolicy { max_age: Some(Duration::from_secs(60)), deadline: Some(Duration::from_secs(120)) };
    control.activate_release(next(&fixture, "second"), policy).unwrap();
    control.activate_release(next(&fixture, "third"), DrainPolicy::default()).unwrap();
    let state = control.state().unwrap();
    let drain = |name: &str| state.releases[name].drain.clone();
    assert!(drain("third").is_none());
    let (build, second) = (drain("build").unwrap(), drain("second").unwrap());
    // The first drain keeps its limits, and the release drained by the second activation has none.
    assert_eq!(build.stops_at, Some(build.since + 120_000));
    assert_eq!((second.stops_at, second.reconnects_until), (None, None));
    control.activate_release(next(&fixture, "second"), DrainPolicy::default()).unwrap();
    let state = control.state().unwrap();
    assert!(state.releases["second"].drain.is_none() && state.releases["third"].drain.is_some());
    fixture.close().await;
}
