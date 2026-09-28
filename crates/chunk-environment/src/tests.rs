use super::*;
use chunk_contract::{BackendConnection, ControlConnection};
use chunk_proto::{
    sync::v1::{CallRequest, JvmRegistration, SubscribeRequest, core_client::CoreClient},
    v1::backend_client::BackendClient,
};
use prost::Message;
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
            bundle: Some(path),
            environment: "test".into(),
            backend_record: state.join("backend.json"),
            control_record: state.join("control.json"),
            state,
            backend_bind: "127.0.0.1:0".parse().unwrap(),
            control_bind: "127.0.0.1:0".parse().unwrap(),
            fresh: false,
        },
        gateway: GatewayConfig::new(bind),
        management: None,
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
    let mut client = CoreClient::connect(control.endpoint).await.unwrap();
    let nodes = SubscribeRequest { topic: "nodes".into(), ..SubscribeRequest::default() };
    let mut updates = client.subscribe(authorized(nodes, &control.token)).await.unwrap().into_inner();
    let snapshot = updates.message().await.unwrap().unwrap();
    assert!(snapshot.snapshot && snapshot.error.is_none());
    drop(updates);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert!(!backend_record.exists());
    assert!(!control_record.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_core_keeps_its_gateway_id() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateways = Vec::new();
    for _ in 0..2 {
        let config = config(directory.path(), "127.0.0.1:0".parse().unwrap()).core;
        let core = Core::start(config, |_| {}).await.unwrap();
        gateways.push(core.target().unwrap().gateway);
        core.stop(|| {}).await.unwrap();
    }
    assert_eq!(gateways[0].id, gateways[1].id);
    assert_ne!(gateways[0].credential, gateways[1].credential);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_deletes_control_files_only_once_surviving_jvms_have_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), "127.0.0.1:0".parse().unwrap()).core;
    config.fresh = true;
    let nodes = config.state.join("control").join("nodes");
    std::fs::create_dir_all(&nodes).unwrap();
    // A JVM of the previous session still holds its launch marker's lock, and never re-attaches.
    let marker = nodes.join("5f1d3c9e-2a4b-4c8d-9e6f-0a1b2c3d4e5f.launch");
    std::fs::write(&marker, b"{}").unwrap();
    let lock = std::fs::File::open(&marker).unwrap();
    lock.try_lock().unwrap();
    let mut jvm = std::process::Command::new("sleep").arg("60").stdin(lock).spawn().unwrap();

    let starting = tokio::spawn(Core::start(config, |_| {}));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!starting.is_finished());
    assert!(marker.exists());
    jvm.kill().unwrap();
    jvm.wait().unwrap();
    let core = tokio::time::timeout(Duration::from_secs(30), starting).await.unwrap().unwrap().unwrap();
    assert!(!marker.exists());
    core.stop(|| {}).await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_refuses_a_backend_on_the_address_surviving_jvms_re_attach_at() {
    let directory = tempfile::tempdir().unwrap();
    let previous = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut config = config(directory.path(), "127.0.0.1:0".parse().unwrap()).core;
    (config.fresh, config.backend_bind) = (true, previous);
    let nodes = config.state.join("control").join("nodes");
    std::fs::create_dir_all(&nodes).unwrap();
    let marker = nodes.join("5f1d3c9e-2a4b-4c8d-9e6f-0a1b2c3d4e5f.launch");
    let launch = serde_json::json!({"process_id": "jvm", "generation": 1, "token_sha256": "",
        "control_endpoint": format!("http://{previous}")});
    std::fs::write(&marker, launch.to_string()).unwrap();
    let lock = std::fs::File::open(&marker).unwrap();
    lock.try_lock().unwrap();
    let mut jvm = std::process::Command::new("sleep").arg("60").stdin(lock).spawn().unwrap();

    let started = tokio::time::timeout(Duration::from_secs(30), Core::start(config, |_| {})).await.unwrap();
    assert!(started.is_err());
    assert!(marker.exists());
    jvm.kill().unwrap();
    jvm.wait().unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_stops_a_survivor_that_re_attaches_at_its_launch_records_address_once_that_is_free() {
    stops_a_survivor_at_its_previous_control_address(true).await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_stops_a_survivor_without_a_recorded_address_at_the_previous_discovery_address() {
    stops_a_survivor_at_its_previous_control_address(false).await;
}

#[cfg(target_os = "linux")]
async fn stops_a_survivor_at_its_previous_control_address(recorded: bool) {
    let directory = tempfile::tempdir().unwrap();
    let mut config = config(directory.path(), "127.0.0.1:0".parse().unwrap()).core;
    config.fresh = true;
    // The survivor's control served elsewhere; another process holds that address for now.
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let previous = occupied.local_addr().unwrap();
    let endpoint = format!("http://{previous}");
    let nodes = config.state.join("control").join("nodes");
    std::fs::create_dir_all(&nodes).unwrap();
    let record = if recorded {
        // A stale discovery record names neither that address nor a credential this start has.
        ControlConnection { endpoint: "http://127.0.0.1:1".into(), token: "stale".into() }
    } else {
        // An older launch record names no address, so the previous control's discovery record is its only evidence.
        ControlConnection { endpoint: endpoint.clone(), token: "previous".into() }
    };
    std::fs::write(&config.control_record, serde_json::to_vec(&record).unwrap()).unwrap();
    let id = "5f1d3c9e-2a4b-4c8d-9e6f-0a1b2c3d4e5f";
    let marker = nodes.join(format!("{id}.launch"));
    // The launch record of process `survivor`, whose credential is `survivor-credential`.
    let digest = "f81f7f42445b7b8d50607fb2be1213427da19382f3aa9bbd41d0c56c050fcc74";
    let mut launch = serde_json::json!({"process_id": "survivor", "generation": 1, "token_sha256": digest});
    if recorded {
        launch["control_endpoint"] = endpoint.clone().into();
    }
    std::fs::write(&marker, launch.to_string()).unwrap();
    let lock = std::fs::File::open(&marker).unwrap();
    lock.try_lock().unwrap();
    let mut jvm = std::process::Command::new("sleep").arg("60").stdin(lock).spawn().unwrap();
    // As at launch, its PID and start time are recorded, which is how control kills a JVM it did not spawn.
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", jvm.id())).unwrap();
    let started: u64 = stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().parse().unwrap();
    let pid = serde_json::json!({"pid": jvm.id(), "started": started});
    std::fs::write(nodes.join(format!("{id}.pid")), pid.to_string()).unwrap();
    // Whether the marker and previous discovery record were unchanged the last time they were seen while it ran.
    let evidence: Vec<_> =
        [&marker, &config.control_record].map(|path| (path.clone(), std::fs::read(path).unwrap())).into();
    let watched = tokio::task::spawn_blocking(move || {
        let mut unchanged = true;
        loop {
            let seen = evidence.iter().all(|(path, bytes)| std::fs::read(path).ok().as_ref() == Some(bytes));
            if jvm.try_wait().unwrap().is_some() {
                return unchanged;
            }
            unchanged = seen;
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let registration = JvmRegistration {
        process_id: "survivor".into(),
        generation: 1,
        app: "app".into(),
        profile: "small".into(),
        artifact_digest: "artifact".into(),
        deployment: "previous".into(),
        player_endpoint: "127.0.0.1:1".into(),
        protocol: 776,
    };
    // Like a JVM, it retries registering at the endpoint it was launched with, until control answers.
    tokio::spawn(async move {
        loop {
            let attempt = async {
                let mut client = CoreClient::connect(endpoint.clone()).await.ok()?;
                let call = CallRequest {
                    method: "chunk:register".into(),
                    arguments: registration.encode_to_vec(),
                    ..CallRequest::default()
                };
                client.call(authorized(call, "survivor-credential")).await.ok()
            };
            if let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(500), attempt).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    let starting = tokio::spawn(Core::start(config, |_| {}));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!watched.is_finished());
    drop(occupied);
    let core = tokio::time::timeout(Duration::from_secs(30), starting).await.unwrap().unwrap().unwrap();
    assert!(watched.await.unwrap());
    assert!(!marker.exists());
    assert_ne!(core.control_connection().unwrap().endpoint, format!("http://{previous}"));
    core.stop(|| {}).await.unwrap();
}
