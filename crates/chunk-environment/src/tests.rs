use super::*;
use chunk_contract::{BackendConnection, ControlConnection};
use chunk_proto::v1::{WatchRequest, backend_client::BackendClient, local_control_client::LocalControlClient};
use std::{collections::BTreeMap, net::SocketAddr, path::Path};

fn config(directory: &Path, bind: SocketAddr) -> Config {
    let bundle = chunk_contract::Deployment {
        contracts: chunk_contract::Contracts::default(),
        contract_version: 2,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: "test".into(),
        source: "export const value=1;".into(),
        tables: BTreeMap::new(),
        functions: BTreeMap::new(),
    };
    let path = directory.join("bundle.json");
    std::fs::write(&path, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let state = directory.join("state");
    Config {
        services: Services::default(),
        core: CoreConfig {
            bundle: path,
            environment: "test".into(),
            backend_record: state.join("backend.json"),
            control_record: state.join("control.json"),
            state,
            backend_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            fresh: false,
        },
        gateway: GatewayConfig::new(bind),
    }
}

fn authorized<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

#[test]
fn services_accept_only_the_all_in_one_layouts() {
    assert_eq!("core,gateway,exec".parse::<Services>().unwrap(), Services::default());
    let services = "exec, core".parse::<Services>().unwrap();
    assert!(services.contains(Service::Core) && services.contains(Service::Exec));
    assert!(!services.contains(Service::Gateway));
    for rejected in ["", "core", "core,gateway", "gateway,exec", "core,exec,jvm"] {
        assert!(rejected.parse::<Services>().is_err(), "{rejected:?} was accepted");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_gateway_bind_releases_core() {
    let directory = tempfile::tempdir().unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for _ in 0..2 {
        let config = config(directory.path(), occupied.local_addr().unwrap());
        let (backend, control) = (config.core.backend_record.clone(), config.core.control_record.clone());
        let error = tokio::time::timeout(Duration::from_secs(30), run(config, CancellationToken::new()))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(!backend.exists());
        assert!(!control.exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_in_one_serves_backend_and_control_until_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let config = config(directory.path(), "127.0.0.1:0".parse().unwrap());
    let (backend_record, control_record) = (config.core.backend_record.clone(), config.core.control_record.clone());
    let stop = CancellationToken::new();
    let running = tokio::spawn(run(config, stop.clone()));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !control_record.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let backend: BackendConnection = chunk_service::read(&backend_record).unwrap();
    let mut request = authorized((), &backend.token);
    request.metadata_mut().insert("x-chunk-environment", "test".parse().unwrap());
    request.metadata_mut().insert("x-chunk-deployment", "test".parse().unwrap());
    BackendClient::connect(backend.endpoint).await.unwrap().check_deployment(request).await.unwrap();
    let control: ControlConnection = chunk_service::read(&control_record).unwrap();
    let mut client = LocalControlClient::connect(control.endpoint).await.unwrap();
    let watch = client.watch(authorized(WatchRequest { proxy_id: "test".into() }, &control.token));
    let snapshot = watch.await.unwrap().into_inner().message().await.unwrap().unwrap();
    assert!(snapshot.snapshot);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert!(!backend_record.exists());
    assert!(!control_record.exists());
}
