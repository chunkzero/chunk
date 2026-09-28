//! A `RunnerHost`, whose launcher here only records its calls and holds them while a test does, while each test plays
//! the runner and its JVM.

use super::*;
use crate::core::{LaunchSpec, Launcher, READINESS, RunnerConfig, runner::RunnerHost};
use chunk_control::Host as _;
use chunk_proto::sync::v1::NodePhase;
use std::collections::BTreeSet;
use tokio::sync::{RwLock, watch};

const OTHER: &str = "runner-2";

#[derive(Clone, Debug, PartialEq)]
enum Call {
    Launch { host: String, credential: String, spec: LaunchSpec },
    Release(String),
}

/// Records each call, which tests await. A call returns only once no test holds its kind.
#[derive(Default)]
struct Machines {
    calls: watch::Sender<Vec<Call>>,
    launches: RwLock<()>,
    releases: RwLock<()>,
}

#[tonic::async_trait]
impl Launcher for Machines {
    async fn launch(&self, id: &str, credential: &str, spec: &LaunchSpec) -> std::io::Result<()> {
        let call = Call::Launch { host: id.into(), credential: credential.into(), spec: spec.clone() };
        self.calls.send_modify(|calls| calls.push(call));
        drop(self.launches.read().await);
        Ok(())
    }

    async fn release(&self, id: &str) -> std::io::Result<bool> {
        self.calls.send_modify(|calls| calls.push(Call::Release(id.into())));
        drop(self.releases.read().await);
        Ok(true)
    }
}

impl Machines {
    fn calls(&self) -> Vec<Call> {
        self.calls.borrow().clone()
    }

    /// The calls once one satisfies `made`.
    async fn wait(&self, made: impl Fn(&Call) -> bool) -> Vec<Call> {
        let mut calls = self.calls.subscribe();
        let found = tokio::time::timeout(Duration::from_secs(10), calls.wait_for(|calls| calls.iter().any(&made)));
        found.await.expect("the launcher was called").unwrap().clone()
    }
}

fn runner_host(machines: &Arc<Machines>, readiness: Duration) -> Arc<RunnerHost> {
    let config = RunnerConfig { readiness, ..RunnerConfig::new(machines.clone()) };
    Arc::new(RunnerHost::new("test", config))
}

/// Attaches `runner` to `fixture`'s core, which keeps `RELEASE`'s archive.
fn attach(fixture: &Fixture, runner: &RunnerHost) {
    runner.attach(&fixture.control, Issuer::new("test", None, &fixture.cli), fixture.network.clone());
    let path = fixture.directory.path().join("release.tar.gz");
    std::fs::write(&path, b"archive").unwrap();
    let archive = ReleaseArchive { path, sha256: auth::hex(&Sha256::digest(b"archive")), size: 7 };
    fixture.archives.insert(RELEASE.into(), archive);
}

async fn start(readiness: Duration) -> (Fixture, Arc<RunnerHost>, Arc<Machines>) {
    let machines = Arc::new(Machines::default());
    let runner = runner_host(&machines, readiness);
    let fixture = Fixture::with_host(runner.clone()).await;
    attach(&fixture, &runner);
    (fixture, runner, machines)
}

fn credential(fixture: &Fixture) -> String {
    Issuer::new("test", None, &fixture.cli).machine(MachineKind::Jvm, HOST)
}

/// The release `HOST` runs, whose archive core keeps.
fn release() -> chunk_control::Release {
    chunk_control::Release { artifact_digest: RELEASE.into(), ..super::super::runtime::release() }
}

async fn ensure(runner: &RunnerHost) -> Progress {
    runner.ensure(HOST, &release(), "bridge", "small").await.unwrap()
}

/// `call`'s outcome, which must not stall.
async fn settled<T>(call: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), call).await.expect("the call settled").unwrap()
}

/// The registration of the JVM `launch` starts.
fn jvm(launch: &JvmLaunch) -> JvmRegistration {
    JvmRegistration {
        process_id: launch.process_id.clone(),
        generation: launch.generation,
        app: launch.app.clone(),
        profile: launch.profile.clone(),
        artifact_digest: "digest".into(),
        deployment: launch.deployment.clone(),
        player_endpoint: "127.0.0.1:1".into(),
        protocol: 776,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ensure_launches_one_machine_whose_jvm_registers_and_becomes_ready() {
    let (fixture, runner, machines) = start(READINESS).await;
    for _ in 0..3 {
        assert!(matches!(ensure(&runner).await, Progress::Pending));
    }
    let credential = credential(&fixture);
    let spec = LaunchSpec {
        core_endpoint: fixture.network.clone(),
        environment: "test".into(),
        player_address: None,
        memory_mib: 512,
    };
    assert_eq!(machines.calls(), [Call::Launch { host: HOST.into(), credential: credential.clone(), spec }]);

    let launch = result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    assert_eq!((launch.release_id.as_str(), launch.app.as_str(), launch.generation), (RELEASE, "bridge", 1));
    let registered = fixture.runner_call(&credential, "chunk:register", &jvm(&launch)).await;
    assert_eq!(result::<JvmRegistered>(&registered).host, HOST);
    let Progress::Ready(connection) = ensure(&runner).await else { panic!("the JVM registered") };
    assert_eq!((connection.identity.process_id, connection.token), (launch.process_id, credential));
    assert_eq!(machines.calls().len(), 1);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_that_misses_the_readiness_deadline_fails_its_host() {
    let (fixture, runner, machines) = start(Duration::ZERO).await;
    let Progress::Failed(reason) = ensure(&runner).await else { panic!("the deadline passed") };
    assert!(reason.contains("did not register"), "{reason}");
    assert!(matches!(ensure(&runner).await, Progress::Failed(_)));
    assert_eq!(machines.calls().len(), 1);

    let credential = credential(&fixture);
    let launch = result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    assert_eq!(code(&fixture.runner_call(&credential, "chunk:register", &jvm(&launch)).await), Code::Stopped);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_that_differs_from_its_hosts_launch_is_denied() {
    let (fixture, runner, _) = start(READINESS).await;
    ensure(&runner).await;
    let credential = credential(&fixture);
    let launch = result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    for changed in [
        JvmRegistration { process_id: "another".into(), ..jvm(&launch) },
        JvmRegistration { generation: 2, ..jvm(&launch) },
        JvmRegistration { app: "another".into(), ..jvm(&launch) },
        JvmRegistration { artifact_digest: "another".into(), ..jvm(&launch) },
    ] {
        assert_eq!(code(&fixture.runner_call(&credential, "chunk:register", &changed).await), Code::Denied);
    }
    assert!(matches!(ensure(&runner).await, Progress::Pending));
    result::<JvmRegistered>(&fixture.runner_call(&credential, "chunk:register", &jvm(&launch)).await);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_launched_before_core_restarted_re_attaches() {
    let (fixture, runner, _) = start(READINESS).await;
    ensure(&runner).await;
    let credential = credential(&fixture);
    let launch = result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    // No host row holds the launch, so stopping core leaves its JVM running, as a crash would.
    let machines = Arc::new(Machines::default());
    let restarted = runner_host(&machines, READINESS);
    let fixture = fixture.restart(restarted.clone()).await;
    attach(&fixture, &restarted);
    assert_eq!(restarted.unowned().unwrap(), BTreeSet::from([HOST.to_owned()]));
    assert!(restarted.unresolved(HOST) && !restarted.stopped(HOST));

    // The runner's boot stays bound, and its JVM registers as it did before.
    assert_eq!(result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await), launch);
    let registered = fixture.runner_call(&credential, "chunk:register", &jvm(&launch)).await;
    assert_eq!(result::<JvmRegistered>(&registered).host, HOST);
    assert!(restarted.unowned().unwrap().is_empty() && !restarted.unresolved(HOST));
    assert!(matches!(ensure(&restarted).await, Progress::Ready(_)));
    assert!(machines.calls().is_empty());
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn release_stops_the_machine_and_revokes_its_credential() {
    let (fixture, runner, machines) = start(READINESS).await;
    ensure(&runner).await;
    let credential = credential(&fixture);
    let launch = result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    result::<JvmRegistered>(&fixture.runner_call(&credential, "chunk:register", &jvm(&launch)).await);
    assert!(!runner.stopped(HOST));

    assert!(runner.release(HOST).await.unwrap());
    assert_eq!(machines.calls().last(), Some(&Call::Release(HOST.into())));
    assert!(runner.stopped(HOST) && runner.connection(HOST).is_none());
    assert!(fixture.control.launch(HOST).is_none());
    let message = CallRequest {
        method: "chunk:launch".into(),
        arguments: JvmBoot { boot: "boot-1".into() }.encode_to_vec(),
        ..CallRequest::default()
    };
    let status = fixture.client.clone().call(authorized(message, &credential)).await.unwrap_err();
    assert_eq!(status.code(), tonic::Code::Unauthenticated);
    // A released host never launches again.
    assert!(matches!(ensure(&runner).await, Progress::Failed(_)));
    assert_eq!(machines.calls().len(), 2);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_that_boots_again_fails_its_host() {
    let (fixture, _runner, machines) = start(READINESS).await;
    fixture.control.activate_release(release()).unwrap();
    let control = fixture.control.clone();
    let claim = tokio::spawn(async move { control.claim(super::super::runtime::login()).await });
    let calls = machines.wait(|call| matches!(call, Call::Launch { .. })).await;
    let Call::Launch { host, credential, .. } = calls[0].clone() else { panic!("the first call launches") };

    result::<JvmLaunch>(&fixture.launch(&credential, "boot-1").await);
    assert_eq!(code(&fixture.launch(&credential, "boot-2").await), Code::Denied);
    // Control fails the host and releases it through its launcher.
    machines.wait(|call| *call == Call::Release(host.clone())).await;
    let status = fixture.control.nodes().unwrap().into_iter().find(|node| node.host == host);
    assert!(status.is_none_or(|node| matches!(node.phase, NodePhase::Stopping | NodePhase::Stopped)));
    claim.abort();
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_release_during_a_stalled_launch_cancels_it_then_stops_the_machine() {
    let (fixture, runner, machines) = start(READINESS).await;
    let held = machines.launches.write().await;
    let ensuring = tokio::spawn({
        let runner = runner.clone();
        async move { ensure(&runner).await }
    });
    machines.wait(|call| matches!(call, Call::Launch { .. })).await;
    let releasing = tokio::spawn({
        let runner = runner.clone();
        async move { runner.release(HOST).await }
    });
    // The release takes the host's turn only once the launch it cancelled has ended, so the machine it stops is the
    // one the launch may have started.
    let Progress::Failed(reason) = settled(ensuring).await else { panic!("the release cancelled the launch") };
    assert!(reason.contains("released"), "{reason}");
    assert!(settled(releasing).await.unwrap());
    drop(held);
    assert!(matches!(machines.calls().as_slice(), [Call::Launch { .. }, Call::Release(host)] if host == HOST));
    assert!(runner.stopped(HOST) && fixture.control.launch(HOST).is_none());
    assert!(matches!(ensure(&runner).await, Progress::Failed(_)));
    assert_eq!(machines.calls().len(), 2);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_host_released_before_it_launched_never_launches_even_after_a_restart() {
    let (fixture, runner, machines) = start(READINESS).await;
    // Nothing ran there, so the release is confirmed at once, and the ensure that arrives after it launches nothing.
    assert!(runner.release(HOST).await.unwrap());
    assert!(matches!(ensure(&runner).await, Progress::Failed(_)));
    // While a stalled release holds another host's turn, that host doesn't launch either.
    assert!(matches!(runner.ensure(OTHER, &release(), "bridge", "small").await.unwrap(), Progress::Pending));
    let held = machines.releases.write().await;
    let releasing = tokio::spawn({
        let runner = runner.clone();
        async move { runner.release(OTHER).await }
    });
    machines.wait(|call| *call == Call::Release(OTHER.into())).await;
    let Progress::Failed(_) = runner.ensure(OTHER, &release(), "bridge", "small").await.unwrap() else {
        panic!("a released host doesn't launch")
    };
    drop(held);
    assert!(settled(releasing).await.unwrap());
    assert!(matches!(machines.calls().as_slice(), [Call::Launch { host, .. }, Call::Release(_)] if host == OTHER));

    let restarted = runner_host(&Arc::new(Machines::default()), READINESS);
    let fixture = fixture.restart(restarted.clone()).await;
    attach(&fixture, &restarted);
    assert!(matches!(ensure(&restarted).await, Progress::Failed(_)));
    assert!(fixture.control.launch(HOST).is_none());
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_launcher_calls_time_out_and_the_host_is_still_released() {
    let machines = Arc::new(Machines::default());
    let config = RunnerConfig {
        readiness: Duration::from_millis(200),
        release_timeout: Duration::from_millis(200),
        ..RunnerConfig::new(machines.clone())
    };
    let runner = Arc::new(RunnerHost::new("test", config));
    let fixture = Fixture::with_host(runner.clone()).await;
    attach(&fixture, &runner);
    let launches = machines.launches.write().await;
    let Progress::Failed(reason) = ensure(&runner).await else { panic!("the launch timed out") };
    assert!(reason.contains("did not finish"), "{reason}");
    drop(launches);

    // The launch may have started a machine, so the release stops it, trying again after one that stalls.
    let stalled = machines.releases.write().await;
    assert!(!runner.release(HOST).await.unwrap());
    assert!(!runner.stopped(HOST));
    drop(stalled);
    assert!(runner.release(HOST).await.unwrap());
    let release = Call::Release(HOST.into());
    assert!(
        matches!(machines.calls().as_slice(), [Call::Launch { .. }, first, second] if *first == release && *second == release)
    );
    assert!(runner.stopped(HOST));
    fixture.stop().await;
}
