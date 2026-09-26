use super::*;
use crate::state::Capacity;

#[tokio::test]
async fn placement_commits_capacity_before_any_host_call_and_cancel_never_launches() {
    let fixture = Fixture::new().await;
    let path = fixture.directory.path().join("control.sqlite");
    let control = open(&path, fixture.config.clone(), fixture.host.clone()).unwrap();
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

    control.cancel(waiting).await.unwrap();
    assert!(control.state().unwrap().claims["waiting"].phase == Phase::Released);
    assert!(fixture.host.ids.lock().unwrap().is_empty());

    let executor = Executor::start(&control);
    eventually(|| control.state().unwrap().hosts[&host].capacity == Capacity::Ready).await;
    assert_eq!(*fixture.host.ids.lock().unwrap(), BTreeSet::from([host]));
    executor.stop().await;
    fixture.close().await;
}
