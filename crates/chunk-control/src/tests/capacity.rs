use super::*;
use crate::state::Capacity;

#[tokio::test]
async fn placement_commits_capacity_before_any_host_call_and_cancel_never_launches() {
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("control.sqlite");
    let control = open(&path, fixture.release.clone(), fixture.host.clone()).unwrap();
    let waiting = request("waiting", &uuid::Uuid::new_v4().to_string());
    let claim = tokio::spawn({
        let (control, waiting) = (control.clone(), waiting.clone());
        async move { control.claim(waiting).await }
    });
    eventually(|| control.state().unwrap().claims.contains_key("waiting")).await;
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["waiting"].session].host.clone();
    assert_eq!(state.hosts[&host].capacity, Capacity::Requested);
    assert!(fixture.host.ids.lock().unwrap().is_empty());
    assert!(!claim.is_finished());
    claim.abort();
    assert!(claim.await.unwrap_err().is_cancelled());

    // Without a JVM to confirm the withdrawal, cancelling leaves the claim withdrawing, and launches nothing.
    assert!(control.cancel(waiting).await.is_err());
    assert!(control.state().unwrap().claims["waiting"].phase == Phase::Withdrawing);
    assert!(fixture.host.ids.lock().unwrap().is_empty());

    let (executor, jvm) = (Executor::start(&control), CancellationToken::new());
    let follower = tokio::spawn(follow(control.clone(), fixture.host.clone(), jvm.clone()));
    eventually(|| control.state().unwrap().hosts[&host].capacity == Capacity::Ready).await;
    assert_eq!(*fixture.host.ids.lock().unwrap(), BTreeSet::from([host]));
    eventually(|| control.state().unwrap().claims["waiting"].phase == Phase::Released).await;
    executor.stop().await;
    jvm.cancel();
    follower.await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn a_restart_resumes_requested_and_releasing_capacity() {
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("control.sqlite");
    let control = open(&path, fixture.release.clone(), fixture.host.clone()).unwrap();
    let claim = tokio::spawn({
        let control = control.clone();
        async move { control.claim(request("waiting", &uuid::Uuid::new_v4().to_string())).await }
    });
    eventually(|| control.state().unwrap().claims.contains_key("waiting")).await;
    claim.abort();
    let _ = claim.await;
    let state = control.state().unwrap();
    let requested = state.sessions[&state.claims["waiting"].session].host.clone();
    let releasing = uuid::Uuid::new_v4().to_string();
    control
        .update(|state| {
            let host = crate::state::HostState {
                capacity: Capacity::Releasing,
                retired: true,
                ..crate::state::HostState::requested("build", "bridge", "local")
            };
            state.hosts.insert(releasing.clone(), host);
            Ok(())
        })
        .unwrap();
    drop(control);

    let control = fixture.control().await;
    eventually(|| {
        let state = control.state().unwrap();
        state.hosts[&requested].capacity == Capacity::Ready && state.released(&releasing)
    })
    .await;
    assert_eq!(*fixture.host.ids.lock().unwrap(), BTreeSet::from([requested]));
    assert_eq!(*fixture.host.terminated.lock().unwrap(), BTreeSet::from([releasing]));
    fixture.close().await;
}

#[tokio::test]
async fn capacity_is_released_only_once_its_host_confirms_the_runtime_exited() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let assignment = control.claim(request("active", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    fixture.arrive(&control, "active").await;
    control.activate(assignment.claim.unwrap()).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["active"].session].host.clone();
    fixture.host.unconfirmed.store(true, Ordering::Release);
    let command = chunk_proto::control::v1::ShutdownNodeRequest {
        operation_id: "stop".into(),
        host_id: host.clone(),
        timeout_seconds: 0,
    };
    control.shutdown_node(&command).unwrap();
    control.progress_drains().unwrap();
    // The executor keeps asking the host, but an unconfirmed exit changes nothing.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let state = control.state().unwrap();
    assert_eq!(state.hosts[&host].capacity, Capacity::Releasing);
    assert!(state.claims["active"].phase == Phase::Arrived);

    fixture.host.unconfirmed.store(false, Ordering::Release);
    eventually(|| control.state().unwrap().released(&host)).await;
    assert!(control.state().unwrap().claims["active"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn shutdown_stops_the_host_of_a_placement_committing_while_it_drains() {
    let fixture = Fixture::new();
    let control =
        open(&fixture.directory.path().join("control.sqlite"), fixture.release.clone(), fixture.host.clone()).unwrap();
    let (committing, paused) = std::sync::mpsc::channel();
    let (resume, resumed) = std::sync::mpsc::channel();
    *control.authority.committing.lock().unwrap() = Some(Box::new(move || {
        committing.send(()).unwrap();
        resumed.recv().unwrap();
    }));
    let claim = tokio::spawn({
        let control = control.clone();
        async move { control.claim(request("late", &uuid::Uuid::new_v4().to_string())).await }
    });
    // The placement passed the draining check and reserved a new host, but has not published it.
    tokio::task::spawn_blocking(move || paused.recv().unwrap()).await.unwrap();
    let shutdown = tokio::spawn({
        let control = control.clone();
        async move { control.shutdown().await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!shutdown.is_finished());
    resume.send(()).unwrap();
    shutdown.await.unwrap().unwrap();
    let state = control.state().unwrap();
    let host = &state.sessions[&state.claims["late"].session].host;
    assert!(fixture.host.terminated.lock().unwrap().contains(host));
    claim.abort();
    fixture.close().await;
}
