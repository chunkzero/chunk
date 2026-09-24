use super::*;
use std::{collections::BTreeMap, time::Duration};

fn staged(directory: &std::path::Path, id: &str) -> Staged {
    let release = Release { apps: vec![], id: id.into(), directory: directory.join(id), archive: None };
    let control = chunk_control::Config {
        contracts: chunk_control::Contracts::default(),
        apps: BTreeMap::from([("bridge".into(), super::super::tests::app("bridge", "local", 4))]),
        deployment: chunk_proto::v1::DeploymentRef { environment: "local".into(), deployment: id.into() },
        artifact_digest: id.into(),
        profiles: BTreeMap::from([(
            "local".into(),
            chunk_control::MachineProfile { memory_mib: 512, max_sessions: 4 },
        )]),
        session_types: BTreeMap::from([(
            "bridge/default".into(),
            chunk_control::SessionType { app: "bridge".into(), machine_profile: "local".into(), capacity: 4 },
        )]),
        max_processes: 4,
        idle_node_timeout_seconds: chunk_control::DEFAULT_IDLE_NODE_TIMEOUT_SECONDS,
    };
    let bundle = chunk_contract::Deployment {
        contracts: chunk_contract::Contracts::default(),
        contract_version: 2,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: "export const value=1;".into(),
        tables: BTreeMap::new(),
        functions: BTreeMap::new(),
    };
    Staged { release, java: "unused".into(), control, bundle }
}

fn settings(state: &std::path::Path, bind: SocketAddr) -> Settings {
    Settings {
        state: state.into(),
        bind,
        backend_bind: "127.0.0.1:0".parse().unwrap(),
        control_bind: "127.0.0.1:0".parse().unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_proxy_start_releases_earlier_services() {
    let directory = tempfile::tempdir().unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let settings = settings(directory.path(), occupied.local_addr().unwrap());
    for _ in 0..2 {
        let error = tokio::time::timeout(
            Duration::from_secs(30),
            start(&settings, staged(directory.path(), "test"), &Reporter::new().0),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(!settings.state.join("backend.json").exists());
        assert!(!settings.state.join("control/test/connection.json").exists());
    }
}
