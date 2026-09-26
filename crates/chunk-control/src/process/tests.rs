use super::*;
use std::os::unix::fs::PermissionsExt;

fn manifest_jar(manifest: &str) -> Vec<u8> {
    let mut jar = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    jar.start_file("META-INF/MANIFEST.MF", zip::write::SimpleFileOptions::default()).unwrap();
    jar.write_all(manifest.as_bytes()).unwrap();
    jar.finish().unwrap().into_inner()
}

#[tokio::test]
async fn launch_registration_is_frozen_and_only_owned_children_can_be_terminated() {
    let directory = tempfile::tempdir().unwrap();
    let java = directory.path().join("java");
    std::fs::write(&java, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut artifact = crate::tests::test_app();
    let library = manifest_jar("Manifest-Version: 1.0\r\n\r\n");
    let library_path = directory.path().join(format!("libs/{:x}.jar", Sha256::digest(&library)));
    std::fs::create_dir_all(directory.path().join("libs")).unwrap();
    std::fs::write(&library_path, &library).unwrap();
    let launcher = manifest_jar(&format!(
        "Manifest-Version: 1.0\r\nClass-Path: {}\r\n\r\n",
        library_path.strip_prefix(directory.path()).unwrap().display()
    ));
    std::fs::write(directory.path().join(&artifact.jar), &launcher).unwrap();
    artifact.sha256 = format!("{:x}", Sha256::digest(&launcher));
    let host = ProcessHost::new(ProcessHostConfig {
        distribution: directory.path().into(),
        java,
        directory: directory.path().join("nodes"),
        deployment: chunk_proto::v1::DeploymentRef { environment: "test".into(), deployment: "build".into() },
        apps: BTreeMap::from([("bridge".into(), artifact)]),
        profiles: BTreeMap::from([
            ("local".into(), crate::MachineProfile { memory_mib: 512, max_sessions: 2 }),
            ("large".into(), crate::MachineProfile { memory_mib: 1024, max_sessions: 2 }),
        ]),
        backend: chunk_contract::BackendConnection {
            platform_token: None,
            environment: "test".into(),
            deployment: "build".into(),
            endpoint: "http://127.0.0.1:1".into(),
            token: "unused".into(),
        },
    });
    host.configure("http://127.0.0.1:1".into()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let process = host.launch(&id, "bridge", "local").unwrap();
    // Destinations may host an app's session on a profile other than the session's default.
    host.launch(&uuid::Uuid::new_v4().to_string(), "bridge", "large").unwrap();
    host.prune(&BTreeSet::new()).unwrap();
    assert!(host.process(&id).unwrap().is_some());
    let registration = ProcessRegistration {
        identity: Some(process.identity.clone()),
        control_endpoint: "http://127.0.0.1:1".into(),
        player_endpoint: "127.0.0.1:2".into(),
    };
    let token = format!("Bearer {}", process.token);
    assert!(host.connection(&id).is_none());
    assert!(host.register("wrong-token", registration.clone()).is_err());
    assert!(
        host.register(
            &token,
            ProcessRegistration {
                identity: Some(ProcessIdentity { app_id: "changed".into(), ..process.identity.clone() }),
                ..registration.clone()
            }
        )
        .is_err()
    );
    assert_eq!(host.register(&token, registration.clone()).unwrap(), process.identity);
    assert_eq!(host.register(&token, registration.clone()).unwrap(), process.identity);
    assert!(
        host.register(&token, ProcessRegistration { player_endpoint: "127.0.0.1:3".into(), ..registration }).is_err()
    );
    assert!(host.ensure(&id, "changed", "local").await.is_err());
    assert!(host.ensure(&id, "bridge", "local").await.is_ok());
    host.terminate(&id).await.unwrap();
    assert!(host.stopped(&id));
    assert!(host.ensure(&id, "bridge", "local").await.is_err());
    host.terminate(&id).await.unwrap();
    let stale = uuid::Uuid::new_v4().to_string();
    std::fs::write(host.path(&stale, "launch").unwrap(), b"").unwrap();
    assert!(host.terminate(&stale).await.is_err());
    assert!(!host.stopped(&stale));
    let invalid = uuid::Uuid::new_v4().to_string();
    let jar = directory.path().join("app.jar");
    std::fs::write(&jar, b"changed artifact").unwrap();
    assert!(host.ensure(&invalid, "bridge", "local").await.is_err());
    assert!(!host.path(&invalid, "launch").unwrap().exists());
    host.terminate(&invalid).await.unwrap();
    assert!(host.stopped(&invalid));
    std::fs::write(&jar, &launcher).unwrap();
    assert!(matches!(host.ensure(&invalid, "bridge", "local").await, Err(Error::Stopped)));
    // A classpath JAR replaced under its digest name no longer matches the app identity.
    let replaced = uuid::Uuid::new_v4().to_string();
    std::fs::write(&library_path, manifest_jar("Manifest-Version: 1.0\r\nCreated-By: replacement\r\n\r\n")).unwrap();
    assert!(matches!(
        host.ensure(&replaced, "bridge", "local").await,
        Err(Error::Invalid("app classpath digest mismatch"))
    ));
    assert!(!host.path(&replaced, "launch").unwrap().exists());
    std::fs::write(&library_path, &library).unwrap();
    let failed_log = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir(host.path(&failed_log, "jvm.log").unwrap()).unwrap();
    assert!(host.ensure(&failed_log, "bridge", "local").await.is_err());
    assert!(host.stopped(&failed_log));
    host.terminate(&failed_log).await.unwrap();
    std::fs::remove_dir(host.path(&failed_log, "jvm.log").unwrap()).unwrap();
    assert_stopped_hosts_are_pruned(&host, &invalid, &stale).await;
}

async fn assert_stopped_hosts_are_pruned(host: &ProcessHost, retained: &str, unconfirmed: &str) {
    host.shutdown().await.unwrap();
    host.prune(&BTreeSet::from([retained.into()])).unwrap();
    assert!(host.stopped(retained));
    host.prune(&BTreeSet::new()).unwrap();
    let processes = host.processes.lock().unwrap();
    assert!(processes.running.is_empty());
    assert!(processes.failed.is_empty());
    assert!(!host.path(retained, "exit").unwrap().exists());
    assert!(host.path(unconfirmed, "launch").unwrap().exists());
}

#[tokio::test]
async fn a_jvm_whose_host_crashed_after_registration_re_attaches_by_its_launch_record() {
    let directory = tempfile::tempdir().unwrap();
    let java = directory.path().join("java");
    std::fs::write(&java, "#!/bin/sh\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut artifact = crate::tests::test_app();
    let jar = manifest_jar("Manifest-Version: 1.0\r\n\r\n");
    std::fs::write(directory.path().join(&artifact.jar), &jar).unwrap();
    artifact.sha256 = format!("{:x}", Sha256::digest(&jar));
    let config = || ProcessHostConfig {
        distribution: directory.path().into(),
        java: java.clone(),
        directory: directory.path().join("nodes"),
        deployment: chunk_proto::v1::DeploymentRef { environment: "test".into(), deployment: "build".into() },
        apps: BTreeMap::from([("bridge".into(), artifact.clone())]),
        profiles: BTreeMap::from([("local".into(), crate::MachineProfile { memory_mib: 512, max_sessions: 2 })]),
        backend: chunk_contract::BackendConnection {
            platform_token: None,
            environment: "test".into(),
            deployment: "build".into(),
            endpoint: "http://127.0.0.1:1".into(),
            token: "unused".into(),
        },
    };
    let crashed = ProcessHost::new(config());
    crashed.configure("http://127.0.0.1:1".into()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let process = crashed.launch(&id, "bridge", "local").unwrap();
    let registration = ProcessRegistration {
        identity: Some(process.identity.clone()),
        control_endpoint: "http://127.0.0.1:1".into(),
        player_endpoint: "127.0.0.1:2".into(),
    };
    crashed.register(&format!("Bearer {}", process.token), registration.clone()).unwrap();
    // Control dies after acknowledging registration and before committing anything; the JVM survives.
    std::mem::forget(crashed);

    let host = ProcessHost::new(config());
    assert!(host.unresolved(&id));
    assert!(host.adopt("another-credential", registration.clone()).is_err());
    let mut changed = registration.clone();
    changed.identity.as_mut().unwrap().process_id = "another-process".into();
    assert!(host.adopt(&process.token, changed).is_err());
    assert!(host.unresolved(&id));
    host.adopt(&process.token, registration.clone()).unwrap();
    assert!(!host.unresolved(&id));
    assert_eq!(host.connection(&id).unwrap().token, process.token);
    assert!(host.adopt(&process.token, registration).is_err());
    assert!(matches!(host.terminate(&id).await, Err(Error::Unresolved(_))));
    assert!(!host.stopped(&id));
}

#[tokio::test]
async fn a_launch_marker_without_a_record_is_never_adopted() {
    let directory = tempfile::tempdir().unwrap();
    let host = ProcessHost::new(ProcessHostConfig {
        distribution: directory.path().into(),
        java: directory.path().join("java"),
        directory: directory.path().join("nodes"),
        deployment: chunk_proto::v1::DeploymentRef { environment: "test".into(), deployment: "build".into() },
        apps: BTreeMap::from([("bridge".into(), crate::tests::test_app())]),
        profiles: BTreeMap::from([("local".into(), crate::MachineProfile { memory_mib: 512, max_sessions: 2 })]),
        backend: chunk_contract::BackendConnection {
            platform_token: None,
            environment: "test".into(),
            deployment: "build".into(),
            endpoint: "http://127.0.0.1:1".into(),
            token: "unused".into(),
        },
    });
    let id = uuid::Uuid::new_v4().to_string();
    let registration = ProcessRegistration {
        identity: Some(ProcessIdentity { runtime_id: id.clone(), app_id: "bridge".into(), ..Default::default() }),
        control_endpoint: "http://127.0.0.1:1".into(),
        player_endpoint: "127.0.0.1:2".into(),
    };
    assert!(host.adopt("credential", registration.clone()).is_err());
    std::fs::create_dir_all(directory.path().join("nodes")).unwrap();
    std::fs::write(host.path(&id, "launch").unwrap(), b"").unwrap();
    assert!(host.adopt("credential", registration).is_err());
    assert!(host.unresolved(&id));
}
