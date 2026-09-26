use super::*;
use chunk_proto::v1::NodePhase;

#[tokio::test]
async fn failed_launch_is_stopped_not_reused_and_cleaned_up_after_recovery() {
    let fixture = Fixture::new().await;
    let host_config = || crate::ProcessHostConfig {
        distribution: fixture.directory.path().into(),
        java: "unused-java".into(),
        directory: fixture.directory.path().join("nodes"),
        deployment: fixture.config.deployment.clone(),
        apps: fixture.config.apps.clone(),
        profiles: fixture.config.profiles.clone(),
        backend: chunk_contract::BackendConnection {
            environment: "test".into(),
            deployment: "build".into(),
            endpoint: "http://127.0.0.1:1".into(),
            token: "unused".into(),
            platform_token: None,
        },
    };
    let host = Arc::new(crate::ProcessHost::new(host_config()));
    host.configure("http://127.0.0.1:1".into()).unwrap();
    let path = fixture.directory.path().join("launch.sqlite");
    let control = open(&path, fixture.config.clone(), host.clone()).unwrap();
    let executor = Executor::start(&control);
    // The app JAR is missing, so launch fails before a marker or child exists.
    for operation in ["first", "second"] {
        assert!(matches!(
            control.claim(request(operation, &uuid::Uuid::new_v4().to_string())).await,
            Err(Error::Stopped)
        ));
        let state = control.state().unwrap();
        assert!(state.hosts.values().all(|host| host.retired && host.failure.is_some()));
        assert!(state.sessions.values().all(|session| session.retired));
        assert!(state.drains.is_empty());
        assert!(control.nodes().unwrap().nodes.iter().all(|node| node.phase == NodePhase::Stopped as i32));
    }
    assert_eq!(control.nodes().unwrap().nodes.len(), 2);
    executor.stop().await;
    drop(control);
    drop(host);

    let host = Arc::new(crate::ProcessHost::new(host_config()));
    host.configure("http://127.0.0.1:1".into()).unwrap();
    let control = open(&path, fixture.config.clone(), host.clone()).unwrap();
    let executor = Executor::start(&control);
    assert!(control.nodes().unwrap().nodes.iter().all(|node| node.phase == NodePhase::Stopped as i32));
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims.values().all(|claim| claim.phase == Phase::Released));
    assert!(state.sessions.is_empty());
    assert!(state.drains.is_empty());
    assert!(state.hosts.is_empty());
    assert!(control.nodes().unwrap().nodes.is_empty());
    assert_eq!(std::fs::read_dir(host_config().directory).unwrap().count(), 0);

    for operation in ["third", "fourth"] {
        let claim = request(operation, &uuid::Uuid::new_v4().to_string());
        assert!(matches!(control.claim(claim.clone()).await, Err(Error::Stopped)));
        let nodes = control.nodes().unwrap().nodes;
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].phase, NodePhase::Stopped as i32);
        control.reconcile_all().await.unwrap();
        assert!(control.state().unwrap().hosts.is_empty());
        assert!(!host.stopped(&nodes[0].host_id));
        assert_eq!(std::fs::read_dir(host_config().directory).unwrap().count(), 0);
        assert!(control.claim(claim).await.is_err());
        assert!(control.nodes().unwrap().nodes.is_empty());
    }
    executor.stop().await;
    fixture.close().await;
}
