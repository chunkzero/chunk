use super::*;
use std::os::unix::fs::PermissionsExt;

fn manifest_jar(manifest: &str) -> Vec<u8> {
    let mut jar = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
    jar.start_file("META-INF/MANIFEST.MF", zip::write::SimpleFileOptions::default()).unwrap();
    jar.write_all(manifest.as_bytes()).unwrap();
    jar.finish().unwrap().into_inner()
}

#[tokio::test]
async fn launch_registration_is_frozen_and_only_owned_children_can_be_released() {
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
    let mut release = release(artifact);
    release.profiles.insert("large".into(), crate::MachineProfile { memory_mib: 1024, max_sessions: 2 });
    let host = host(directory.path(), java);
    host.configure("http://127.0.0.1:1".into()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let process = host.launch(&id, &release, "bridge", "local").unwrap().unwrap();
    // Destinations may host an app's session on a profile other than the session's default.
    host.launch(&uuid::Uuid::new_v4().to_string(), &release, "bridge", "large").unwrap();
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
    assert!(matches!(host.ensure(&id, &release, "changed", "local").await, Ok(Progress::Failed(_))));
    assert!(matches!(host.ensure(&id, &release, "bridge", "local").await, Ok(Progress::Ready(_))));
    assert!(host.release(&id).await.unwrap());
    assert!(host.stopped(&id));
    assert!(matches!(host.ensure(&id, &release, "bridge", "local").await, Ok(Progress::Failed(_))));
    assert!(host.release(&id).await.unwrap());
    let stale = uuid::Uuid::new_v4().to_string();
    std::fs::write(host.path(&stale, "launch").unwrap(), b"").unwrap();
    let held = File::open(host.path(&stale, "launch").unwrap()).unwrap();
    held.lock().unwrap();
    assert!(!host.release(&stale).await.unwrap());
    assert!(!host.stopped(&stale));
    let invalid = uuid::Uuid::new_v4().to_string();
    let jar = directory.path().join("app.jar");
    std::fs::write(&jar, b"changed artifact").unwrap();
    assert!(matches!(host.ensure(&invalid, &release, "bridge", "local").await, Ok(Progress::Failed(_))));
    assert!(!host.path(&invalid, "launch").unwrap().exists());
    assert!(host.release(&invalid).await.unwrap());
    assert!(host.stopped(&invalid));
    std::fs::write(&jar, &launcher).unwrap();
    assert!(matches!(host.ensure(&invalid, &release, "bridge", "local").await, Ok(Progress::Failed(_))));
    // A classpath JAR replaced under its digest name no longer matches the app identity.
    let replaced = uuid::Uuid::new_v4().to_string();
    std::fs::write(&library_path, manifest_jar("Manifest-Version: 1.0\r\nCreated-By: replacement\r\n\r\n")).unwrap();
    assert!(matches!(
        host.ensure(&replaced, &release, "bridge", "local").await,
        Ok(Progress::Failed(reason)) if reason.ends_with("app classpath digest mismatch")
    ));
    assert!(!host.path(&replaced, "launch").unwrap().exists());
    std::fs::write(&library_path, &library).unwrap();
    let failed_log = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir(host.path(&failed_log, "jvm.log").unwrap()).unwrap();
    assert!(matches!(host.ensure(&failed_log, &release, "bridge", "local").await, Ok(Progress::Failed(_))));
    assert!(host.stopped(&failed_log));
    assert!(host.release(&failed_log).await.unwrap());
    std::fs::remove_dir(host.path(&failed_log, "jvm.log").unwrap()).unwrap();
    assert_stopped_hosts_are_pruned(&host, &invalid, &stale).await;
    drop(held);
}

async fn assert_stopped_hosts_are_pruned(host: &ProcessHost, retained: &str, unconfirmed: &str) {
    // The unconfirmed launch's JVM may still run.
    assert!(matches!(host.shutdown().await, Err(Error::Unresolved(_))));
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
    // The JVM closes its stdin, as app code calling `System.in.close()` does.
    std::fs::write(&java, "#!/bin/sh\nexec 0<&-\necho $$\nexec sleep 60\n").unwrap();
    std::fs::set_permissions(&java, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut artifact = crate::tests::test_app();
    let jar = manifest_jar("Manifest-Version: 1.0\r\n\r\n");
    std::fs::write(directory.path().join(&artifact.jar), &jar).unwrap();
    artifact.sha256 = format!("{:x}", Sha256::digest(&jar));
    let release = release(artifact);
    let crashed = host(directory.path(), java.clone());
    crashed.configure("http://127.0.0.1:1".into()).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let process = crashed.launch(&id, &release, "bridge", "local").unwrap().unwrap();
    let registration = ProcessRegistration {
        identity: Some(process.identity.clone()),
        control_endpoint: "http://127.0.0.1:1".into(),
        player_endpoint: "127.0.0.1:2".into(),
    };
    crashed.register(&format!("Bearer {}", process.token), registration.clone()).unwrap();
    // Control dies after acknowledging registration and before committing anything; the JVM survives.
    std::mem::forget(crashed);

    let host = host(directory.path(), java);
    assert!(host.unresolved(&id));
    assert_eq!(host.unowned().unwrap(), BTreeSet::from([id.clone()]));
    // Until re-attached, only the launch record knows the JVM's credential.
    assert_eq!((host.authenticate(&process.token), host.unadopted(&process.token)), (None, Some(id.clone())));
    assert!(host.unadopted("another-credential").is_none());
    assert!(host.adopt("another-credential", registration.clone()).is_err());
    let mut changed = registration.clone();
    changed.identity.as_mut().unwrap().process_id = "another-process".into();
    assert!(host.adopt(&process.token, changed).is_err());
    assert!(host.unresolved(&id));
    host.adopt(&process.token, registration.clone()).unwrap();
    assert!(!host.unresolved(&id));
    assert!(host.unowned().unwrap().is_empty());
    assert_eq!((host.authenticate(&process.token), host.unadopted(&process.token)), (Some(id.clone()), None));
    assert_eq!(host.connection(&id).unwrap().token, process.token);
    assert!(host.adopt(&process.token, registration).is_err());
    assert!(!host.stopped(&id));
    // The JVM exits without a Child in this host; its launch marker's lock confirms the termination.
    let log = host.path(&id, "jvm.log").unwrap();
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&log).ok().and_then(|log| log.trim().parse::<u32>().ok()) {
            break pid;
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert!(std::process::Command::new("kill").arg(pid.to_string()).status().unwrap().success());
    assert!(host.release(&id).await.unwrap());
    assert!(host.stopped(&id));
}

/// The test environment's release `build`, running `artifact`.
fn release(artifact: chunk_contract::AppArtifact) -> Release {
    Release {
        apps: BTreeMap::from([("bridge".into(), artifact)]),
        deployment: chunk_proto::v1::DeploymentRef { environment: "test".into(), deployment: "build".into() },
        profiles: BTreeMap::from([("local".into(), crate::MachineProfile { memory_mib: 512, max_sessions: 2 })]),
        ..crate::tests::release()
    }
}

/// A host in `directory` that launches release `build` with `java`.
fn host(directory: &std::path::Path, java: std::path::PathBuf) -> ProcessHost {
    let host = ProcessHost::new(ProcessHostConfig {
        directory: directory.join("nodes"),
        backend: chunk_contract::BackendConnection {
            platform_token: None,
            environment: "test".into(),
            deployment: "build".into(),
            endpoint: "http://127.0.0.1:1".into(),
            token: "unused".into(),
        },
    });
    host.add_release("build", Distribution { directory: directory.into(), java }).unwrap();
    host
}

/// A host that launches nothing in `directory`.
fn idle_host(directory: &tempfile::TempDir) -> ProcessHost {
    host(directory.path(), directory.path().join("java"))
}

/// Publishes a launch marker for a new host ID, returning the lock control holds until it spawns the JVM.
fn marked(host: &ProcessHost, directory: &tempfile::TempDir) -> (String, File) {
    let id = uuid::Uuid::new_v4().to_string();
    let record = LaunchRecord {
        process_id: "jvm".into(),
        generation: 1,
        token_sha256: digest("token"),
        control_endpoint: String::new(),
    };
    std::fs::create_dir_all(directory.path().join("nodes")).unwrap();
    let lock = host.record_launch(&id, &record).unwrap();
    (id, lock)
}

/// Whether `host` confirms `id` stopped within a second. A process another test forks holds a copy of every open lock
/// until it executes its program.
fn confirmed_stopped(host: &ProcessHost, id: &str) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    while !host.stopped(id) {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
}

#[test]
fn a_launch_is_unresolved_while_a_process_holds_its_lock_and_confirmed_exited_once_none_does() {
    let directory = tempfile::tempdir().unwrap();
    let host = idle_host(&directory);
    let (id, lock) = marked(&host, &directory);
    let mut child = std::process::Command::new("sleep").arg("60").stdin(Stdio::from(lock)).spawn().unwrap();
    assert!(!host.stopped(&id));
    assert_eq!(host.unowned().unwrap(), BTreeSet::from([id.clone()]));
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(confirmed_stopped(&host, &id));
    assert!(host.path(&id, "exit").unwrap().is_file());
    assert!(host.unowned().unwrap().is_empty());
}

#[test]
fn a_launch_interrupted_before_its_spawn_is_confirmed_exited() {
    let directory = tempfile::tempdir().unwrap();
    let host = idle_host(&directory);
    let (id, lock) = marked(&host, &directory);
    assert!(host.unresolved(&id));
    // Control stops before spawning, which releases its lock.
    drop(lock);
    assert!(confirmed_stopped(&host, &id));
}

#[test]
fn a_launch_whose_marker_cannot_be_looked_up_stays_unresolved() {
    let directory = tempfile::tempdir().unwrap();
    let host = idle_host(&directory);
    let (id, lock) = marked(&host, &directory);
    drop(lock);
    let nodes = directory.path().join("nodes");
    std::fs::set_permissions(&nodes, std::fs::Permissions::from_mode(0o000)).unwrap();
    let unresolved = host.unresolved(&id);
    std::fs::set_permissions(&nodes, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(unresolved);
}

#[tokio::test]
async fn a_launch_marker_without_a_record_is_never_adopted() {
    let directory = tempfile::tempdir().unwrap();
    let host = idle_host(&directory);
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
}
