//! The `chunk-jvm` image end to end: a managed core launches its JVM there with podman. The first JVM records the app's
//! AOT cache, and a second one starts with it. Run it with `just jvm-e2e`, which sets `CHUNK_E2E_IMAGE` to the image and
//! `CHUNK_E2E_RELEASE` to a release archive of `examples/local`.

use super::*;
use crate::{CommandLauncher, Core, RunnerConfig, managed::Managed};
use chunk_proto::{
    control::v1::{ClaimRequest, Identity, SessionDemand, ShutdownNodeRequest},
    sync::v1::NodePhase,
};
use std::{
    net::{IpAddr, SocketAddr},
    process::{Command, Stdio},
    sync::OnceLock,
};

/// Removes every container this run started, named with its prefix, including after a failure.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let listed = podman(&["ps", "-aq", "--filter", &format!("name=^{}", self.0)]);
        for id in listed.lines() {
            podman(&["rm", "-f", "-t", "0", "--ignore", id]);
        }
    }
}

fn podman(arguments: &[&str]) -> String {
    let output = Command::new("podman").args(arguments).output().expect("podman runs");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// This machine's address on its default route, which must be private.
fn lan_address() -> IpAddr {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
    socket.connect("192.0.2.1:9").unwrap();
    let address = socket.local_addr().unwrap().ip();
    assert!(chunk_service::net::private(address), "{address} is not private");
    address
}

/// Each process in `container` as `(pid, ppid, state, command)`, read from its `/proc`.
fn processes(container: &str) -> Vec<(u32, u32, String, String)> {
    let listed = podman(&["exec", container, "sh", "-c", "cat /proc/[0-9]*/stat 2>/dev/null"]);
    let parse = |stat: &str| {
        let (head, tail) = stat.split_once(" (")?;
        let (command, rest) = tail.rsplit_once(") ")?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?.to_owned();
        Some((head.parse().ok()?, fields.next()?.parse().ok()?, state, command.to_owned()))
    };
    listed.lines().filter_map(parse).collect()
}

/// Player `player`'s login, a claim with the same operation ID.
fn login(player: u8) -> ClaimRequest {
    ClaimRequest {
        operation_id: format!("login-{player}"),
        proxy_id: "proxy".into(),
        connection_id: format!("connection-{player}"),
        identity: Some(Identity {
            uuid: format!("00000000-0000-0000-0000-{player:012}"),
            username: format!("player{player}"),
            properties: vec![],
        }),
        demand: Some(SessionDemand {
            key: "lobby".into(),
            session_type: "lobby/default".into(),
            machine_profile: "local".into(),
        }),
        source: None,
        deployment: String::new(),
    }
}

/// Claims `player`'s login until it is placed, returning how long that took; each attempt waits 35 seconds for a JVM.
async fn placed(control: &chunk_control::Control, player: u8) -> (SocketAddr, Duration) {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(180);
    let assignment = loop {
        match control.claim(login(player)).await {
            Ok(assignment) => break assignment,
            Err(error) => assert!(tokio::time::Instant::now() < deadline, "the claim failed: {error}"),
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    };
    (assignment.preparation.unwrap().endpoint.parse().unwrap(), started.elapsed())
}

/// The one host whose node is online.
async fn online(control: &chunk_control::Control) -> String {
    let online = async {
        loop {
            let nodes = control.nodes().unwrap();
            let mut online = nodes.iter().filter(|node| node.phase == NodePhase::Online);
            if let (Some(node), None) = (online.next(), online.next()) {
                break node.host.clone();
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(30), online).await.expect("one host is online")
}

/// Waits for `container` to exit, returning its exit code. Started while the container runs, since a removed container
/// can't be waited for.
fn exited(container: &str) -> tokio::process::Child {
    tokio::process::Command::new("podman").args(["wait", container]).stdout(Stdio::piped()).spawn().unwrap()
}

async fn exit_code(exit: tokio::process::Child) -> String {
    let exit = tokio::time::timeout(Duration::from_secs(120), exit.wait_with_output()).await.unwrap().unwrap();
    String::from_utf8_lossy(&exit.stdout).trim().to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs podman and the chunk-jvm image; run `just jvm-e2e`"]
async fn a_managed_core_runs_its_jvm_in_the_chunk_jvm_image() {
    let image = std::env::var("CHUNK_E2E_IMAGE").expect("CHUNK_E2E_IMAGE names the image");
    let archive = std::path::PathBuf::from(std::env::var("CHUNK_E2E_RELEASE").expect("CHUNK_E2E_RELEASE is set"));
    // Unique to this run, so a concurrent run's containers are left alone.
    let prefix = format!("chunk-jvm-e2e-{}-", std::process::id());
    let _cleanup = Cleanup(prefix.clone());
    let started = tokio::time::Instant::now();
    let mut harness = Harness::new().await;
    let release_id = archive.file_name().unwrap().to_str().unwrap().trim_end_matches(".tar.gz").to_owned();
    let release = artifact(&harness.management, &harness.url, &release_id, fs::read(&archive).unwrap());
    harness.deploy("dep_a", release);

    // The host network keeps core's peer check real: the runner reaches core from the LAN address it serves at.
    let address = lan_address();
    let free = std::net::TcpListener::bind((address, 0)).unwrap().local_addr().unwrap();
    let config = CoreConfig { core_bind: Some(free), ..harness.core() };
    let sh = |script: &str| vec!["sh".to_owned(), "-c".to_owned(), script.to_owned(), image.clone()];
    // Java logs its AOT cache use, and each container's log is kept once the release removes it.
    let logs = harness.directory.path().join("logs");
    fs::create_dir_all(&logs).unwrap();
    let launcher = CommandLauncher {
        launch: sh(&format!(
            r#"exec podman run -d --rm --network host --name "{prefix}$CHUNK_HOST_ID" -e CHUNK_CORE_ENDPOINT \
            -e CHUNK_JVM_CREDENTIAL -e CHUNK_ENVIRONMENT_ID -e JAVA_TOOL_OPTIONS=-Xlog:aot=info "$0" >/dev/null"#
        )),
        release: sh(&format!(
            r#"podman logs "{prefix}$CHUNK_HOST_ID" > "{logs}/$CHUNK_HOST_ID.log" 2>&1
            exec podman rm -f -t 20 --ignore "{prefix}$CHUNK_HOST_ID" >/dev/null"#,
            logs = logs.display()
        )),
    };
    let core = Core::start_with_launcher(config, RunnerConfig::new(Arc::new(launcher))).await.unwrap();
    let gateway = OnceLock::new();
    let lease = watch::Sender::new(crate::managed::Lease::Waiting);
    let managed =
        Managed::new(&harness.management_config(), lease, "env_test".into(), &harness.state(), &core, &gateway, None);

    let checks = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await;
        let activated = started.elapsed();

        // The claim places a lobby session, whose host core launches.
        let control = core.control().unwrap();
        let (endpoint, recorded) = placed(&control, 1).await;
        assert_eq!(endpoint.ip(), address);
        tokio::net::TcpStream::connect(endpoint).await.expect("the JVM serves players at its endpoint");
        let host = online(&control).await;
        let container = format!("{prefix}{host}");
        let exit = exited(&container);

        // An orphan in the container is reparented to the runner, which reaps it once it exits.
        podman(&["exec", &container, "sh", "-c", "sleep 3 >/dev/null 2>&1 &"]);
        let orphan = processes(&container).into_iter().find(|(_, _, _, command)| command == "sleep");
        assert!(matches!(orphan, Some((_, 1, _, _))), "{orphan:?}");
        tokio::time::sleep(Duration::from_secs(5)).await;
        let processes = processes(&container);
        assert!(processes.iter().any(|(pid, _, _, command)| *pid == 1 && command == "chunk-jvm"), "{processes:?}");
        assert!(processes.iter().all(|(_, _, state, command)| state != "Z" && command != "sleep"), "{processes:?}");

        // Stopping the first node lets its runner create and upload the AOT cache before its container goes.
        let shutdown = ShutdownNodeRequest { operation_id: "first".into(), host_id: host.clone(), timeout_seconds: 0 };
        control.shutdown_node(&shutdown).unwrap();
        assert_eq!(exit_code(exit).await, "0", "the recording runner exits cleanly");
        let log = fs::read_to_string(logs.join(format!("{host}.log"))).unwrap();
        assert!(log.contains("uploaded the AOT cache"), "{log}");
        let kept = harness.state().join("aot").join(&release_id).join("lobby");
        assert_eq!(fs::read_dir(kept).unwrap().count(), 1);

        // The next host of the release fetches it, and Java starts with it.
        let (_, cached) = placed(&control, 2).await;
        let host = online(&control).await;
        let container = format!("{prefix}{host}");
        let log = Command::new("podman").args(["logs", &container]).output().unwrap();
        let log = format!("{}{}", String::from_utf8_lossy(&log.stdout), String::from_utf8_lossy(&log.stderr));
        assert!(log.contains("downloading the AOT cache"), "{log}");
        assert!(log.contains("Using AOT-linked classes: true"), "{log}");
        assert!(!log.contains("[warning][aot]") && !log.contains("[error  ][aot]"), "{log}");
        let exit = exited(&container);
        (container, exit, activated, recorded, cached)
    };
    let (container, exit, activated, recorded, cached) = tokio::select! {
        error = managed.run() => panic!("management stopped: {error}"),
        checks = checks => checks,
    };
    let stopping = tokio::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(60), core.stop(|| {})).await.expect("core stops").unwrap();
    let stopped = stopping.elapsed();
    assert_eq!(exit_code(exit).await, "0", "the runner exits cleanly");
    assert!(!Command::new("podman").args(["container", "exists", &container]).status().unwrap().success());
    println!(
        "activated after {activated:?}; recording JVM placed after {recorded:?}, cached JVM after {cached:?}; stopped in \
         {stopped:?}"
    );
}
