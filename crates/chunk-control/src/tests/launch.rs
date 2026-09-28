use super::*;
use chunk_proto::sync::v1::NodePhase;
use std::path::{Path, PathBuf};

#[tokio::test]
async fn failed_launch_is_stopped_not_reused_and_cleaned_up_after_recovery() {
    let fixture = Fixture::new();
    let host_config = || host_config(fixture.directory.path(), "unused-java".into());
    let host = Arc::new(crate::ProcessHost::new(host_config()));
    host.configure("http://127.0.0.1:1".into()).unwrap();
    let path = fixture.directory.path().join("launch.sqlite");
    let control = open(&path, fixture.release.clone(), host.clone()).unwrap();
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
        assert!(control.nodes().unwrap().iter().all(|node| node.phase == NodePhase::Stopped));
    }
    assert_eq!(control.nodes().unwrap().len(), 2);
    executor.stop().await;
    drop(control);
    drop(host);

    let host = Arc::new(crate::ProcessHost::new(host_config()));
    host.configure("http://127.0.0.1:1".into()).unwrap();
    let control = open(&path, fixture.release.clone(), host.clone()).unwrap();
    let executor = Executor::start(&control);
    assert!(control.nodes().unwrap().iter().all(|node| node.phase == NodePhase::Stopped));
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims.values().all(|claim| claim.phase == Phase::Released));
    assert!(state.sessions.is_empty());
    assert!(state.drains.is_empty());
    assert!(state.hosts.is_empty());
    assert!(control.nodes().unwrap().is_empty());
    assert_eq!(std::fs::read_dir(host_config().directory).unwrap().count(), 0);

    for operation in ["third", "fourth"] {
        let claim = request(operation, &uuid::Uuid::new_v4().to_string());
        assert!(matches!(control.claim(claim.clone()).await, Err(Error::Stopped)));
        let nodes = control.nodes().unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].phase, NodePhase::Stopped);
        control.reconcile_all().await.unwrap();
        assert!(control.state().unwrap().hosts.is_empty());
        assert!(!host.stopped(&nodes[0].host));
        assert_eq!(std::fs::read_dir(host_config().directory).unwrap().count(), 0);
        assert!(control.claim(claim).await.is_err());
        assert!(control.nodes().unwrap().is_empty());
    }
    executor.stop().await;
    fixture.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn a_retained_release_launches_new_jvms_after_a_restart_without_being_activated_again() {
    use sha2::Digest;
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let directory = fixture.directory.path();
    let java = directory.join("java");
    std::fs::write(&java, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut previous = fixture.release.clone();
    previous.deployment.deployment = "previous".into();
    previous.release_id = "previous-release".into();
    let jar = zip::ZipWriter::new(std::io::Cursor::new(Vec::new())).finish().unwrap().into_inner();
    let unpacked = directory.join("releases").join(&previous.release_id);
    std::fs::create_dir_all(&unpacked).unwrap();
    std::fs::write(unpacked.join("app.jar"), &jar).unwrap();
    previous.apps.get_mut("bridge").unwrap().sha256 = format!("{:x}", sha2::Sha256::digest(&jar));
    let path = directory.join("launch.sqlite");
    let host = || {
        let host = Arc::new(crate::ProcessHost::new(host_config(directory, java.clone())));
        host.configure("http://127.0.0.1:1".into()).unwrap();
        host
    };
    let control = open(&path, previous, host()).unwrap();
    control.activate_release(fixture.release.clone()).unwrap();
    drop(control);

    // Control restarts with `build` current; `previous` is never activated again.
    let host = host();
    let control = open(&path, fixture.release.clone(), host.clone()).unwrap();
    let executor = Executor::start(&control);
    let request =
        ClaimRequest { deployment: "previous".into(), ..request("previous", &uuid::Uuid::new_v4().to_string()) };
    let claim = tokio::spawn({
        let control = control.clone();
        async move { control.claim(request).await }
    });
    let launched = |node: &crate::NodeStatus| {
        node.deployment == "previous" && directory.join("nodes").join(&node.host).with_extension("launch").exists()
    };
    eventually(|| control.nodes().unwrap().iter().any(launched)).await;
    assert!(control.nodes().unwrap().iter().all(|node| node.phase != NodePhase::Stopped));
    claim.abort();
    executor.stop().await;
    host.shutdown().await.unwrap();
    fixture.close().await;
}

/// A process host in `directory` that launches apps with `java`.
fn host_config(directory: &Path, java: PathBuf) -> crate::ProcessHostConfig {
    crate::ProcessHostConfig {
        directory: directory.join("nodes"),
        releases: directory.join("releases"),
        java,
        environment: "test".into(),
        private_address: None,
    }
}
