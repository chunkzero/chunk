use super::*;

/// The fixture's release as the next deployment version.
fn next(fixture: &Fixture) -> Release {
    let mut release = fixture.release.clone();
    release.deployment.deployment = "next".into();
    release
}

/// The release of the host serving `operation`'s claim.
fn release_of(control: &Control, operation: &str) -> String {
    let state = control.state().unwrap();
    state.hosts[&state.sessions[&state.claims[operation].session].host].release.clone()
}

/// Logs a new player in under `operation` and waits until the player arrived.
async fn arrived(fixture: &Fixture, control: &Control, operation: &str) -> ClaimRequest {
    let request = request(operation, &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(request.clone()).await.unwrap();
    fixture.arrive(control, operation).await;
    control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
    request
}

#[tokio::test]
async fn new_logins_use_the_current_release_while_earlier_sessions_and_their_moves_stay() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let first = arrived(&fixture, &control, "first").await;
    control.activate_release(next(&fixture)).unwrap();
    arrived(&fixture, &control, "second").await;
    assert_eq!(release_of(&control, "first"), "build");
    assert_eq!(release_of(&control, "second"), "next");
    assert!(control.state().unwrap().claims["first"].phase == Phase::Arrived);
    let destination = control
        .move_player(chunk_proto::v1::MovePlayerRequest {
            expected_source: None,
            expected_connection_id: String::new(),
            operation_id: "move".into(),
            player_id: first.identity.clone().unwrap().uuid,
            demand: Some(SessionDemand { key: "arena".into(), ..first.demand.clone().unwrap() }),
        })
        .unwrap();
    control.claim(destination).await.unwrap();
    assert_eq!(release_of(&control, "move"), "build");
    let nodes = control.nodes().unwrap().nodes;
    let releases: BTreeSet<_> = nodes.iter().map(|node| node.deployment.as_str()).collect();
    assert_eq!(releases, BTreeSet::from(["build", "next"]));
    fixture.close().await;
}

#[tokio::test]
async fn a_retired_release_is_forgotten_once_its_last_host_is_released() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    assert!(control.retire_release("build").is_err());
    control.activate_release(next(&fixture)).unwrap();
    arrived(&fixture, &control, "second").await;
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["first"].session].host.clone();
    eventually(|| control.retire_release("build").unwrap()).await;
    assert!(fixture.host.stopped(&host));
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims["first"].phase == Phase::Released);
    assert!(state.claims["second"].phase == Phase::Arrived);
    assert_eq!(state.releases.keys().collect::<Vec<_>>(), ["next"]);
    assert!(control.retire_release("build").unwrap());
    fixture.close().await;
}

#[tokio::test]
async fn recovery_after_a_restart_keeps_every_live_release() {
    let mut fixture = Fixture::new().await;
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    let next = next(&fixture);
    control.activate_release(next.clone()).unwrap();
    arrived(&fixture, &control, "second").await;
    drop(control);
    // Reopening activates `next` again, which is already current.
    fixture.release = next;
    let control = fixture.control().await;
    let state = control.state().unwrap();
    assert_eq!(state.current.as_deref(), Some("next"));
    assert_eq!(state.releases.len(), 2);
    assert!(state.claims.values().all(|claim| claim.phase == Phase::Arrived));
    assert_eq!(release_of(&control, "first"), "build");
    arrived(&fixture, &control, "third").await;
    assert_eq!(release_of(&control, "third"), "next");
    fixture.close().await;
}
