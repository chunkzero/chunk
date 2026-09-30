use super::*;
use chunk_contract::ControlConnection;
use chunk_proto::sync::v1::{
    CallRequest, GatewayArguments, JvmRegistration, SubscribeRequest, core_client::CoreClient,
};
use prost::Message;
use std::{net::SocketAddr, path::Path};

/// A subscription to `gateway/<id>` as a gateway process.
fn gateway_topic(id: &str) -> SubscribeRequest {
    let arguments = GatewayArguments { instance: "test".into() }.encode_to_vec();
    SubscribeRequest { topic: format!("gateway/{id}"), arguments, ..SubscribeRequest::default() }
}

/// A deployment whose `shared/proxy/status` hook answers with `motd`.
fn bundle(id: &str, motd: &str) -> chunk_contract::Deployment {
    let string = serde_json::json!({"schema": {"type": "string"}});
    let integer = serde_json::json!({"schema": {"type": "integer"}});
    serde_json::from_value(serde_json::json!({
        "contract_version": 2, "runtime_profile": "transactional_v1", "id": id, "tables": {},
        "source": format!("export function status() {{ return {{motd: {motd:?}, online: 0, max: 8}}; }}"),
        "functions": {"shared/proxy/status": {
            "kind": "query", "visibility": "public", "export": "status",
            "arguments": {"type": "object", "fields": {"host": string}},
            "result": {"type": "object", "fields": {"motd": string, "online": integer, "max": integer}}
        }}
    }))
    .unwrap()
}

/// A release of `deployment` in environment `test`.
fn release(deployment: &str) -> chunk_control::Release {
    serde_json::from_value(serde_json::json!({
        "apps": {"lobby": {"id": "lobby", "jar": "lobby.jar", "sha256": "digest", "java_version": 25,
            "sessions": {"default": {"machine_profile": "small", "capacity": 8}}}},
        "deployment": {"environment": "test", "deployment": deployment}, "release_id": "digest",
        "profiles": {"small": {"memory_mib": 512, "max_sessions": 2}},
        "session_types": {"lobby/default": {"app": "lobby", "machine_profile": "small", "capacity": 8}},
        "max_processes": 1, "idle_node_timeout_seconds": 0
    }))
    .unwrap()
}

fn core_config(directory: &Path) -> CoreConfig {
    let path = directory.join("bundle.json");
    std::fs::write(&path, serde_json::to_vec(&bundle("test", "Serving test")).unwrap()).unwrap();
    let state = directory.join("state");
    CoreConfig {
        bundle: Some(path),
        environment: "test".into(),
        control_record: state.join("control.json"),
        state,
        control_bind: "127.0.0.1:0".parse().unwrap(),
        core_bind: None,
        private_address: None,
        java: "java".into(),
        environment_token: None,
        fresh: false,
        replication: None,
    }
}

fn all_in_one(core: CoreConfig, bind: SocketAddr) -> Config {
    Config::Core { core: Box::new(core), gateway: Some(GatewayConfig::new(bind)), management: None }
}

/// Runs a gateway machine for `core` on a loopback port until `stop`, returning its task and the address its listener
/// binds, once it does.
fn gateway_machine(
    core: RemoteCore,
    stop: CancellationToken,
) -> (JoinHandle<io::Result<()>>, tokio::sync::mpsc::UnboundedReceiver<SocketAddr>) {
    let (listening, addresses) = tokio::sync::mpsc::unbounded_channel();
    let config = GatewayConfig::new("127.0.0.1:0".parse().unwrap());
    let running = tokio::spawn(gateway::run_remote(core, config, stop, move |address| _ = listening.send(address)));
    (running, addresses)
}

/// The address a gateway machine's listener binds.
async fn listening(addresses: &mut tokio::sync::mpsc::UnboundedReceiver<SocketAddr>) -> SocketAddr {
    tokio::time::timeout(Duration::from_secs(30), addresses.recv()).await.unwrap().unwrap()
}

/// The server list message a status ping to `address` returns, or `None` while nothing there answers within 5 seconds.
async fn motd(address: SocketAddr) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let response = tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = tokio::net::TcpStream::connect(address).await.ok()?;
        // A handshake for protocol 776 that asks for status, then the status request.
        let mut handshake = vec![0x00, 0x88, 0x06, 9];
        handshake.extend_from_slice(b"localhost");
        handshake.extend_from_slice(&address.port().to_be_bytes());
        handshake.push(0x01);
        let mut packets = vec![u8::try_from(handshake.len()).unwrap()];
        packets.extend(handshake);
        packets.extend([0x01, 0x00]);
        stream.write_all(&packets).await.ok()?;
        stream.shutdown().await.ok()?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.ok()?;
        Some(response)
    })
    .await
    .ok()??;
    let response = String::from_utf8_lossy(&response);
    let status: serde_json::Value = serde_json::from_str(&response[response.find('{')?..]).ok()?;
    status["description"]["text"].as_str().map(str::to_owned)
}

/// Waits until a status ping to `address` returns `expected`.
async fn until_motd(address: SocketAddr, expected: Option<&str>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let motd = motd(address).await;
        if motd.as_deref() == expected {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "the gateway answers {motd:?}, not {expected:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn authorized<T>(message: T, token: &str) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

#[test]
fn services_accept_core_and_gateway_and_ignore_a_legacy_exec() {
    assert_eq!("gateway, core".parse::<Services>().unwrap(), Services::default());
    assert_eq!("core,gateway,exec".parse::<Services>().unwrap(), Services::default());
    let core = "core,exec".parse::<Services>().unwrap();
    assert!(core.contains(Service::Core) && !core.contains(Service::Gateway));
    for rejected in ["", "exec", "core,jvm", "core,,gateway"] {
        assert!(rejected.parse::<Services>().is_err(), "{rejected:?} was accepted");
    }
    let gateway = "gateway".parse::<Services>().unwrap();
    assert!(gateway.contains(Service::Gateway) && !gateway.contains(Service::Core));
}

#[test]
fn offline_logins_reach_the_gateway_only_when_enabled() {
    // Reads the environment in a child process, since setting variables in this one needs `unsafe`.
    const CHILD: &str = "CHUNK_OFFLINE_LOGINS_TEST_EXPECTED";
    if let Ok(expected) = std::env::var(CHILD) {
        let Config::Gateway { gateway, .. } = Config::from_env().unwrap() else { panic!("expected a gateway") };
        assert_eq!(gateway.offline_logins.to_string(), expected);
        return;
    }
    for (value, expected) in [(Some("1"), true), (Some("true"), false), (None, false)] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "tests::offline_logins_reach_the_gateway_only_when_enabled"])
            .env(CHILD, expected.to_string())
            .env("CHUNK_SERVICES", "gateway")
            .env("CHUNK_ENVIRONMENT_ID", "test")
            .env("CHUNK_CORE_ENDPOINT", "http://127.0.0.1:7070")
            .env("CHUNK_GATEWAY_CREDENTIAL", "credential")
            .env_remove("CHUNK_OFFLINE_LOGINS")
            .stdout(std::process::Stdio::null());
        if let Some(value) = value {
            child.env("CHUNK_OFFLINE_LOGINS", value);
        }
        assert!(child.status().unwrap().success(), "CHUNK_OFFLINE_LOGINS={value:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_gateway_bind_releases_core() {
    let directory = tempfile::tempdir().unwrap();
    let occupied = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    for _ in 0..2 {
        let core = core_config(directory.path());
        let control = core.control_record.clone();
        let config = all_in_one(core, occupied.local_addr().unwrap());
        let error = tokio::time::timeout(Duration::from_secs(30), run(config, CancellationToken::new()))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
        assert!(!control.exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_binds_its_network_listener_only_when_configured_and_mints_stable_gateway_credentials() {
    for bind in [None, Some("127.0.0.1:0".parse().unwrap())] {
        let directory = tempfile::tempdir().unwrap();
        let mut config = core_config(directory.path());
        config.core_bind = bind;
        let core = Core::start(config, || {}).await.unwrap();
        assert_eq!(core.network_address().is_some(), bind.is_some());
        let credential = core.gateway_credential("remote").unwrap();
        assert_eq!(core.gateway_credential("remote").unwrap(), credential);
        core.revoke_gateway("remote").unwrap();
        assert!(core.gateway_credential("remote").is_err());
        core.stop(|| {}).await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gateway_credentials_and_their_revocation_survive_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let start = || {
        let mut config = core_config(directory.path());
        config.core_bind = Some("127.0.0.1:0".parse().unwrap());
        Core::start(config, || {})
    };
    let core = start().await.unwrap();
    let active = core.gateway_credential("active").unwrap();
    let revoked = core.gateway_credential("revoked").unwrap();
    core.revoke_gateway("revoked").unwrap();
    core.stop(|| {}).await.unwrap();

    let core = start().await.unwrap();
    let mut client = CoreClient::connect(format!("http://{}", core.network_address().unwrap())).await.unwrap();
    let gateway = |id: &str| gateway_topic(id);
    let mut updates = client.subscribe(authorized(gateway("active"), &active)).await.unwrap().into_inner();
    assert!(updates.message().await.unwrap().unwrap().error.is_none());
    let refused = client.subscribe(authorized(gateway("revoked"), &revoked)).await.unwrap_err();
    assert_eq!(refused.code(), tonic::Code::Unauthenticated);
    assert!(core.gateway_credential("revoked").is_err());
    drop(updates);
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_machine_follows_the_current_deployment_until_its_credential_is_revoked() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = core_config(directory.path());
    config.core_bind = Some("127.0.0.1:0".parse().unwrap());
    let core = Core::start(config, || {}).await.unwrap();
    let credential = core.gateway_credential("remote").unwrap();
    let remote = |environment: &str| RemoteCore {
        endpoint: format!("http://{}", core.network_address().unwrap()),
        credential: credential.clone(),
        environment: environment.into(),
    };
    assert_eq!(remote("test").gateway().unwrap().id, "remote");
    let foreign =
        Config::Gateway { gateway: GatewayConfig::new("127.0.0.1:0".parse().unwrap()), core: remote("other") };
    let error = run(foreign, CancellationToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("belongs to environment \"test\""), "{error}");

    // Core grants the credential its own gateway's topic only.
    let mut client = CoreClient::connect(format!("http://{}", core.network_address().unwrap())).await.unwrap();
    for (id, granted) in [("remote", true), ("other", false)] {
        let topic = gateway_topic(id);
        let mut updates = client.subscribe(authorized(topic, &credential)).await.unwrap().into_inner();
        let first = tokio::time::timeout(Duration::from_secs(30), updates.message()).await.unwrap();
        assert_eq!(first.unwrap().unwrap().error.is_none(), granted);
    }

    let stop = CancellationToken::new();
    let (running, mut addresses) = gateway_machine(remote("test"), stop.clone());
    // No release is current, so the gateway takes no logins yet.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(addresses.is_empty());
    core.control().unwrap().activate_release(release("test")).unwrap();
    let address = listening(&mut addresses).await;
    until_motd(address, Some("Serving test")).await;
    core.deploy(bundle("next", "Serving next")).await.unwrap();
    core.control().unwrap().activate_release(release("next")).unwrap();
    until_motd(address, Some("Serving next")).await;

    core.revoke_gateway("remote").unwrap();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert!(!stop.is_cancelled());
    assert_eq!(motd(address).await, None);
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_machine_follows_core_across_restarts_until_core_rejects_its_credential() {
    let directory = tempfile::tempdir().unwrap();
    let start = |core_bind| {
        let mut config = core_config(directory.path());
        config.core_bind = core_bind;
        Core::start(config, || {})
    };
    let core = start(Some("127.0.0.1:0".parse().unwrap())).await.unwrap();
    let network = core.network_address();
    core.control().unwrap().activate_release(release("test")).unwrap();
    let remote = RemoteCore {
        endpoint: format!("http://{}", network.unwrap()),
        credential: core.gateway_credential("remote").unwrap(),
        environment: "test".into(),
    };
    let (running, mut addresses) = gateway_machine(remote, CancellationToken::new());
    let address = listening(&mut addresses).await;
    until_motd(address, Some("Serving test")).await;

    // Once core is back at the same address, the gateway follows it again.
    core.stop(|| {}).await.unwrap();
    let core = start(network).await.unwrap();
    core.deploy(bundle("next", "Serving next")).await.unwrap();
    core.control().unwrap().activate_release(release("next")).unwrap();
    until_motd(address, Some("Serving next")).await;

    // Core revokes the credential while the gateway can't reach it, so it rejects the gateway's next subscription.
    core.stop(|| {}).await.unwrap();
    let core = start(None).await.unwrap();
    core.revoke_gateway("remote").unwrap();
    core.stop(|| {}).await.unwrap();
    let core = start(network).await.unwrap();
    let error = tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("rejected the gateway credential"), "{error}");
    assert_eq!(motd(address).await, None);
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn all_in_one_serves_backend_and_control_until_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let core = core_config(directory.path());
    let (state, control_record) = (core.state.clone(), core.control_record.clone());
    let stop = CancellationToken::new();
    let running = tokio::spawn(run(all_in_one(core, "127.0.0.1:0".parse().unwrap()), stop.clone()));
    tokio::time::timeout(Duration::from_secs(30), async {
        while !control_record.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let control: ControlConnection = chunk_service::read(&control_record).unwrap();
    let mut client = CoreClient::connect(control.endpoint).await.unwrap();
    let nodes = SubscribeRequest { topic: "nodes".into(), ..SubscribeRequest::default() };
    let mut updates = client.subscribe(authorized(nodes, &control.token)).await.unwrap().into_inner();
    let snapshot = updates.message().await.unwrap().unwrap();
    assert!(snapshot.snapshot && snapshot.error.is_none());
    assert!(!state.join("backend.json").exists());
    drop(updates);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert!(!control_record.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_core_keeps_its_gateway_id() {
    let directory = tempfile::tempdir().unwrap();
    let mut gateways = Vec::new();
    for _ in 0..2 {
        let config = core_config(directory.path());
        let core = Core::start(config, || {}).await.unwrap();
        gateways.push(core.target().unwrap().gateway);
        core.stop(|| {}).await.unwrap();
    }
    assert_eq!(gateways[0].id, gateways[1].id);
    assert_ne!(gateways[0].credential, gateways[1].credential);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_deletes_control_files_only_once_surviving_jvms_have_stopped() {
    deletes_control_files_only_once_surviving_jvms_have_stopped(false).await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_with_a_launcher_still_stops_surviving_local_jvms() {
    deletes_control_files_only_once_surviving_jvms_have_stopped(true).await;
}

#[cfg(unix)]
async fn deletes_control_files_only_once_surviving_jvms_have_stopped(launcher: bool) {
    let directory = tempfile::tempdir().unwrap();
    let mut config = core_config(directory.path());
    config.fresh = true;
    let nodes = config.state.join("control").join("nodes");
    std::fs::create_dir_all(&nodes).unwrap();
    // A JVM of the previous session still holds its launch marker's lock, and never re-attaches.
    let marker = nodes.join("5f1d3c9e-2a4b-4c8d-9e6f-0a1b2c3d4e5f.launch");
    std::fs::write(&marker, b"{}").unwrap();
    let lock = std::fs::File::open(&marker).unwrap();
    lock.try_lock().unwrap();
    let mut jvm = std::process::Command::new("sleep").arg("60").stdin(lock).spawn().unwrap();

    let starting = if launcher {
        let machines = std::sync::Arc::new(Machines::default());
        tokio::spawn(Core::start_with_launcher(config, RunnerConfig::new(machines)))
    } else {
        tokio::spawn(Core::start(config, || {}))
    };
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!starting.is_finished());
    assert!(marker.exists());
    jvm.kill().unwrap();
    jvm.wait().unwrap();
    let core = tokio::time::timeout(Duration::from_secs(30), starting).await.unwrap().unwrap().unwrap();
    assert!(!marker.exists());
    core.stop(|| {}).await.unwrap();
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
    let mut config = core_config(directory.path());
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

    let starting = tokio::spawn(Core::start(config, || {}));
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!watched.is_finished());
    drop(occupied);
    let core = tokio::time::timeout(Duration::from_secs(30), starting).await.unwrap().unwrap().unwrap();
    assert!(watched.await.unwrap());
    assert!(!marker.exists());
    assert_ne!(core.control_connection().unwrap().endpoint, format!("http://{previous}"));
    core.stop(|| {}).await.unwrap();
}

const REMOTE: &str = "remote-1";

/// Records each machine release, which it confirms only while `stops` is set.
#[derive(Default)]
struct Machines {
    stops: std::sync::atomic::AtomicBool,
    released: std::sync::Mutex<Vec<String>>,
}

#[tonic::async_trait]
impl Launcher for Machines {
    async fn launch(
        &self,
        _: &str,
        _: &str,
        _: &LaunchSpec,
        _: &tokio_util::sync::CancellationToken,
    ) -> std::io::Result<()> {
        Err(std::io::Error::other("launches nothing"))
    }

    async fn release(&self, id: &str) -> std::io::Result<bool> {
        self.released.lock().unwrap().push(id.into());
        Ok(self.stops.load(std::sync::atomic::Ordering::Relaxed))
    }
}

/// Records a launch on `REMOTE` whose machine runs until something releases it.
fn launched(core: &Core) {
    let launch = chunk_control::Launch {
        deployment: "test".into(),
        release: "release-1".into(),
        app: "lobby".into(),
        profile: "small".into(),
        process_id: "process-1".into(),
        generation: 1,
        boot: None,
    };
    core.control().unwrap().record_launch(REMOTE, launch).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fresh_start_stops_every_recorded_remote_machine_before_forgetting_it() {
    let directory = tempfile::tempdir().unwrap();
    // Local JVMs ignore remote launch records, so this core leaves the machine running, as a crashed one would.
    let core = Core::start(core_config(directory.path()), || {}).await.unwrap();
    launched(&core);
    core.stop(|| {}).await.unwrap();
    // Only a launcher can stop the machine, so a fresh start without one refuses and keeps its record.
    let mut config = core_config(directory.path());
    config.fresh = true;
    let Err(error) = Core::start(config, || {}).await else { panic!("the remote machine may still run") };
    assert!(error.to_string().contains("launcher"), "{error}");

    let machines = std::sync::Arc::new(Machines::default());
    let fresh = || {
        let mut config = core_config(directory.path());
        config.fresh = true;
        Core::start_with_launcher(config, RunnerConfig::new(machines.clone()))
    };
    // A machine whose stop isn't confirmed keeps its record, and core doesn't start.
    assert!(fresh().await.is_err());
    assert_eq!(*machines.released.lock().unwrap(), [REMOTE]);
    machines.stops.store(true, std::sync::atomic::Ordering::Relaxed);
    let core = fresh().await.unwrap();
    assert_eq!(*machines.released.lock().unwrap(), [REMOTE, REMOTE]);
    assert!(core.control().unwrap().launch(REMOTE).is_none());
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn core_stops_remote_machines_once_its_store_stopped() {
    let directory = tempfile::tempdir().unwrap();
    let machines = std::sync::Arc::new(Machines::default());
    machines.stops.store(true, std::sync::atomic::Ordering::Relaxed);
    let config = core_config(directory.path());
    let core = Core::start_with_launcher(config, RunnerConfig::new(machines.clone())).await.unwrap();
    launched(&core);
    let backend = core.backend().unwrap();
    tokio::task::spawn_blocking(move || backend.stop()).await.unwrap();
    assert!(core.control().unwrap().store_stopped());

    // Control can't record the release, yet the machine stops, once, and so does core, whatever control reports.
    let stopped = tokio::time::timeout(Duration::from_secs(30), core.stop(|| {})).await;
    stopped.expect("core stopped").ok();
    assert_eq!(*machines.released.lock().unwrap(), [REMOTE]);
}
