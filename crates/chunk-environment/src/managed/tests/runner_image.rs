//! The `chunk-jvm` image end to end: a managed core launches its JVM there with podman. Run it with `just jvm-e2e`,
//! which sets `CHUNK_E2E_IMAGE` to the image and `CHUNK_E2E_RELEASE` to a release archive of `examples/local`.

use super::*;
use crate::{CommandLauncher, Core, RunnerConfig, managed::Managed};
use chunk_proto::{
    control::v1::{ClaimRequest, Identity, SessionDemand},
    sync::v1::NodePhase,
};
use std::{
    net::{IpAddr, SocketAddr},
    process::{Command, Stdio},
    sync::OnceLock,
};

const CONTAINERS: &str = "chunk-jvm-e2e-";

/// Removes every e2e container, including after a failure.
struct Cleanup;

impl Drop for Cleanup {
    fn drop(&mut self) {
        let listed = podman(&["ps", "-aq", "--filter", &format!("name=^{CONTAINERS}")]);
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

fn login() -> ClaimRequest {
    ClaimRequest {
        operation_id: "login".into(),
        proxy_id: "proxy".into(),
        connection_id: "connection".into(),
        identity: Some(Identity {
            uuid: "00000000-0000-0000-0000-000000000001".into(),
            username: "player".into(),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs podman and the chunk-jvm image; run `just jvm-e2e`"]
async fn a_managed_core_runs_its_jvm_in_the_chunk_jvm_image() {
    let image = std::env::var("CHUNK_E2E_IMAGE").expect("CHUNK_E2E_IMAGE names the image");
    let archive = std::path::PathBuf::from(std::env::var("CHUNK_E2E_RELEASE").expect("CHUNK_E2E_RELEASE is set"));
    let _cleanup = Cleanup;
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
    let launcher = CommandLauncher {
        launch: sh(&format!(
            r#"exec podman run -d --rm --network host --name "{CONTAINERS}$CHUNK_HOST_ID" -e CHUNK_CORE_ENDPOINT \
            -e CHUNK_JVM_CREDENTIAL -e CHUNK_ENVIRONMENT_ID "$0" >/dev/null"#
        )),
        release: sh(&format!(r#"exec podman rm -f -t 20 --ignore "{CONTAINERS}$CHUNK_HOST_ID" >/dev/null"#)),
    };
    let core = Core::start_with_launcher(config, RunnerConfig::new(Arc::new(launcher))).await.unwrap();
    let gateway = OnceLock::new();
    let management = ManagementConfig { url: harness.url.clone(), token: "secret".into() };
    let managed = Managed::new(management, "env_test".into(), &harness.state(), &core, &gateway, None);

    let checks = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await;
        let activated = started.elapsed();

        // The claim places a lobby session, whose host core launches; each attempt waits 35 seconds for its JVM.
        let control = core.control().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(180);
        let assignment = loop {
            match control.claim(login()).await {
                Ok(assignment) => break assignment,
                Err(error) => assert!(tokio::time::Instant::now() < deadline, "the claim failed: {error}"),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        };
        let ready = started.elapsed();
        let endpoint: SocketAddr = assignment.preparation.unwrap().endpoint.parse().unwrap();
        assert_eq!(endpoint.ip(), address);
        tokio::net::TcpStream::connect(endpoint).await.expect("the JVM serves players at its endpoint");
        let online = async {
            loop {
                let nodes = control.nodes().unwrap();
                if let [node] = nodes.as_slice()
                    && node.phase == NodePhase::Online
                {
                    break node.host.clone();
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        };
        let host = tokio::time::timeout(Duration::from_secs(30), online).await.expect("one host is online");
        let container = format!("{CONTAINERS}{host}");
        // Started while the container runs, since a removed container can't be waited for.
        let exit = tokio::process::Command::new("podman").args(["wait", &container]).stdout(Stdio::piped()).spawn();

        // An orphan in the container is reparented to the runner, which reaps it once it exits.
        podman(&["exec", &container, "sh", "-c", "sleep 3 >/dev/null 2>&1 &"]);
        let orphan = processes(&container).into_iter().find(|(_, _, _, command)| command == "sleep");
        assert!(matches!(orphan, Some((_, 1, _, _))), "{orphan:?}");
        tokio::time::sleep(Duration::from_secs(5)).await;
        let processes = processes(&container);
        assert!(processes.iter().any(|(pid, _, _, command)| *pid == 1 && command == "chunk-jvm"), "{processes:?}");
        assert!(processes.iter().all(|(_, _, state, command)| state != "Z" && command != "sleep"), "{processes:?}");

        (container, exit.unwrap(), activated, ready)
    };
    let (container, exit, activated, ready) = tokio::select! {
        error = managed.run() => panic!("management stopped: {error}"),
        checks = checks => checks,
    };
    let stopping = tokio::time::Instant::now();
    core.stop(|| {}).await.unwrap();
    let stopped = stopping.elapsed();
    let exit = tokio::time::timeout(Duration::from_secs(30), exit.wait_with_output()).await.unwrap().unwrap();
    assert_eq!(String::from_utf8_lossy(&exit.stdout).trim(), "0", "the runner exits cleanly");
    assert!(!Command::new("podman").args(["container", "exists", &container]).status().unwrap().success());
    println!("activated after {activated:?}, JVM ready after {ready:?}, stopped in {stopped:?}");
}
