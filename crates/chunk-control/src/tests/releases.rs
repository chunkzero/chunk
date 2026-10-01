use super::*;

/// The fixture's release as the next deployment version, whose sessions hold one player each on up to two JVMs.
fn next(fixture: &Fixture) -> Release {
    let mut release = fixture.release.clone();
    release.deployment.deployment = "next".into();
    release.apps.get_mut("bridge").unwrap().sessions.get_mut("default").unwrap().capacity = 1;
    release.session_types.get_mut("bridge/default").unwrap().capacity = 1;
    release.max_processes = 2;
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
    control.activate(assignment.claim.unwrap()).await.unwrap();
    request
}

#[tokio::test]
async fn new_logins_and_moves_use_the_current_release_while_earlier_sessions_stay() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let first = arrived(&fixture, &control, "first").await;
    control.activate_release(next(&fixture), crate::DrainPolicy::default()).unwrap();
    arrived(&fixture, &control, "second").await;
    assert_eq!(release_of(&control, "first"), "build");
    assert_eq!(release_of(&control, "second"), "next");
    let state = control.state().unwrap();
    assert!(state.claims["first"].phase == Phase::Arrived);
    // Each session takes its capacity from its own release.
    let capacity = |operation: &str| state.sessions[&state.claims[operation].session].capacity;
    assert_eq!((capacity("first"), capacity("second")), (2, 1));
    let destination = control
        .move_player(MoveRequest {
            operation_id: "move".into(),
            player_id: first.identity.clone().unwrap().uuid,
            demand: SessionDemand { key: "arena".into(), ..first.demand.clone().unwrap() },
            source: None,
        })
        .unwrap();
    // A move approved while `build` was current is refused, so its gateway approves it again.
    let refused = control.claim_approved(destination.clone(), "build").await;
    assert!(matches!(refused, Err(Error::Unresolved(crate::ROUTE_AGAIN))));
    assert!(!control.state().unwrap().claims.contains_key("move"));
    control.claim_approved(destination, "next").await.unwrap();
    assert_eq!(release_of(&control, "move"), "next");
    let nodes = control.nodes().unwrap();
    let releases: BTreeSet<_> = nodes.iter().map(|node| node.deployment.as_str()).collect();
    assert_eq!(releases, BTreeSet::from(["build", "next"]));
    fixture.close().await;
}

#[tokio::test]
async fn a_retired_release_is_forgotten_once_its_last_host_is_released() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    assert!(control.retire_release("build").is_err());
    control.activate_release(next(&fixture), crate::DrainPolicy::default()).unwrap();
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
    let mut fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    let next = next(&fixture);
    control.activate_release(next.clone(), crate::DrainPolicy::default()).unwrap();
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

#[tokio::test]
async fn a_login_routed_with_a_replaced_release_is_rejected_for_routing_again() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    control.activate_release(next(&fixture), crate::DrainPolicy::default()).unwrap();
    // Routed with the earlier release before activation switched, a login reserves nothing.
    let routed = |operation: &str, deployment: &str| ClaimRequest {
        deployment: deployment.into(),
        ..request(operation, &uuid::Uuid::new_v4().to_string())
    };
    let rejected = control.claim(routed("second", "build")).await;
    assert!(matches!(rejected, Err(Error::Unresolved(crate::ROUTE_AGAIN))));
    assert!(!control.state().unwrap().claims.contains_key("second"));
    control.claim(routed("second", "next")).await.unwrap();
    assert_eq!(release_of(&control, "second"), "next");
    fixture.close().await;
}

#[tokio::test]
async fn a_release_is_not_retired_while_a_launch_without_a_host_row_may_run_it() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    control.activate_release(next(&fixture), crate::DrainPolicy::default()).unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["first"].session].host.clone();
    // A restore loses the host row while its JVM still holds its launch, before recovery records it again.
    control
        .update(|state| {
            let session = state.claims.remove("first").unwrap().session;
            state.sessions.remove(&session);
            state.players.clear();
            state.hosts.remove(&host);
            Ok(())
        })
        .unwrap();
    fixture.host.forgotten.store(true, Ordering::Release);
    assert!(!control.retire_release("build").unwrap());
    control.retire_idle_hosts().unwrap();
    assert!(control.state().unwrap().releases.contains_key("build"));
    fixture.host.terminated.lock().unwrap().insert(host);
    assert!(control.retire_release("build").unwrap());
    fixture.close().await;
}

#[tokio::test]
async fn an_orphan_whose_release_a_restore_lost_re_attaches_after_another_restart_and_stops() {
    let mut fixture = Fixture::new();
    let control = fixture.control().await;
    arrived(&fixture, &control, "first").await;
    let next = next(&fixture);
    control.activate_release(next.clone(), crate::DrainPolicy::default()).unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["first"].session].host.clone();
    // A restore loses the host, its login and the release it ran, while its JVM keeps running.
    control
        .update(|state| {
            let session = state.claims.remove("first").unwrap().session;
            state.sessions.remove(&session);
            state.players.clear();
            state.hosts.remove(&host);
            state.releases.remove("build");
            Ok(())
        })
        .unwrap();
    drop((state, control));
    fixture.release = next;
    // Control restarts again after recording the orphan but before its JVM confirms it stopped.
    fixture.host.unconfirmed.store(true, Ordering::Release);
    for restart in 0..2 {
        fixture.host.forgotten.store(true, Ordering::Release);
        let control = fixture.control().await;
        control.register_jvm(&host, CREDENTIAL, fixture.host.registration(&host)).unwrap();
        fixture.recovered(&control).await;
        control.reconcile_all().await.unwrap();
        if restart == 0 {
            continue;
        }
        let state = control.state().unwrap();
        assert!(state.hosts[&host].retired && state.hosts[&host].release == "build");
        assert!(!state.releases.contains_key("build"));
        fixture.host.unconfirmed.store(false, Ordering::Release);
        eventually(|| control.state().unwrap().released(&host)).await;
        assert!(fixture.host.terminated.lock().unwrap().contains(&host));
        control.claim(request("relogin", &uuid::Uuid::new_v4().to_string())).await.unwrap();
        assert_eq!(release_of(&control, "relogin"), "next");
    }
    fixture.close().await;
}
