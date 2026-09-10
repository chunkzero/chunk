use super::*;
use std::{collections::BTreeMap, fs, time::Duration};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_edge_start_releases_earlier_services() {
    let directory = tempfile::tempdir().unwrap();
    let artifact = Artifact {
        id: "test".into(),
        directory: directory.path().join("artifact"),
    };
    fs::create_dir(&artifact.directory).unwrap();
    let bundle = chunk_contract::Deployment {
        contract_version: 1,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: artifact.id.clone(),
        source: "export const value=1;".into(),
        tables: BTreeMap::new(),
        functions: BTreeMap::new(),
    };
    fs::write(
        artifact.directory.join("backend.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    let project = Project {
        environment: "local".into(),
        gameplay_distribution: "unused".into(),
        gameplay_module: ":unused".into(),
        profiles: BTreeMap::from([(
            "local".into(),
            chunk_control::MachineProfile {
                memory_mib: 512,
                max_sessions: 4,
            },
        )]),
        session_types: BTreeMap::from([(
            "bridge".into(),
            chunk_control::SessionType {
                machine_profile: "local".into(),
                capacity: 4,
            },
        )]),
        max_processes: 4,
    };
    fs::write(
        directory.path().join("control-config.json"),
        serde_json::to_vec(&chunk_control::Config {
            deployment: chunk_proto::v1::DeploymentRef {
                environment: "local".into(),
                deployment: artifact.id.clone(),
            },
            artifact_digest: artifact.id.clone(),
            profiles: project.profiles.clone(),
            session_types: project.session_types.clone(),
            max_processes: 4,
        })
        .unwrap(),
    )
    .unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let options = Options {
        project: "unused".into(),
        state: directory.path().into(),
        java: "unused".into(),
        bind: occupied.local_addr().unwrap(),
        backend_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
    };
    for _ in 0..2 {
        let mut services = Services::default();
        let error = tokio::time::timeout(Duration::from_secs(10), services.start(&options, &project, &artifact))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(options.state.join("backend.json").exists());
        assert!(options.state.join("control.json").exists());
        tokio::time::timeout(Duration::from_secs(10), services.stop())
            .await
            .unwrap()
            .unwrap();
        assert!(!options.state.join("backend.json").exists());
        assert!(!options.state.join("control.json").exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_bundle_preserves_startup_error() {
    let directory = tempfile::tempdir().unwrap();
    let options = Options {
        project: "unused".into(),
        state: directory.path().into(),
        java: "unused".into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        backend_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
    };
    let project = Project {
        environment: "local".into(),
        gameplay_distribution: "unused".into(),
        gameplay_module: ":unused".into(),
        profiles: BTreeMap::new(),
        session_types: BTreeMap::new(),
        max_processes: 1,
    };
    let artifact = Artifact {
        id: "missing".into(),
        directory: directory.path().join("missing"),
    };
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        run(&options, &project, &artifact, CancellationToken::new()),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::NotFound);
    assert!(!options.state.join("backend.json").exists());
}
