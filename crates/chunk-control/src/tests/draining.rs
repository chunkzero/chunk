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
    control.activate_release(next(&fixture, "next")).unwrap();
    assert!(!control.drain_release("build", DrainPolicy::default()).unwrap());
    control.progress_releases().unwrap();
    assert!(!control.state().unwrap().releases["build"].retired);
    disconnect(&control, first).await;
    // The session type takes no reconnects, so the player logs in to the current release.
    arrived(&fixture, &control, "again", &player, "next").await;
    assert_eq!(release_of(&control, "again"), "next");
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
async fn a_player_reconnects_to_the_session_they_left_on_a_draining_release() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let player = uuid::Uuid::new_v4().to_string();
    let first = arrived(&fixture, &control, "first", &player, "").await;
    control.activate_release(next(&fixture, "next")).unwrap();
    control.drain_release("build", DrainPolicy::default()).unwrap();
    let left = control.state().unwrap().claims["first"].session.clone();
    disconnect(&control, first).await;
    // The release waits for the player within the reconnect grace.
    control.progress_releases().unwrap();
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
    assert_eq!(control.retire_longest_draining(&draining).unwrap(), None);
    assert!(!control.state().unwrap().releases["second"].retired);
    fixture.close().await;
}
