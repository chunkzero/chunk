//! The management launcher against the fake management's capacity requests.

use super::*;
use crate::{
    Core, LaunchSpec, Launcher,
    managed::{Lease, Managed, ManagementLauncher},
};
use chunk_management::v1::{
    Capacity, CapacityState, EnsureCapacityRequest, EnsureCapacityResponse, ReleaseCapacityRequest,
    ReleaseCapacityResponse, Workload,
};
use std::sync::OnceLock;

/// Management's capacity requests, kept by the rules of `packages/management`.
#[derive(Default)]
pub(super) struct Capacities {
    /// Every `EnsureCapacity` call, in order.
    ensures: Vec<EnsureCapacityRequest>,
    requests: BTreeMap<String, Request>,
    /// How many more `EnsureCapacity` replies are lost once management handled their call.
    lost: usize,
    /// Holds the next `EnsureCapacity` call, once recorded, until notified; management then handles it.
    held: Option<Arc<Notify>>,
    /// Refuses the next release of this request for a stale lease, as when management granted a lease core never learns.
    stale: Option<String>,
}

struct Request {
    ensure: EnsureCapacityRequest,
    state: CapacityState,
    message: String,
    /// Whether the reconciler removed the machine.
    torn_down: bool,
}

/// Answers an `EnsureCapacity` or `ReleaseCapacity` call.
pub(super) async fn serve(management: &Management, path: &str, body: &[u8]) -> hyper::Response<Body> {
    let capacity = if path.ends_with("/EnsureCapacity") {
        let ensure = EnsureCapacityRequest::decode(body).unwrap();
        let held = {
            let mut capacities = management.capacity.lock().unwrap();
            capacities.ensures.push(ensure.clone());
            capacities.held.take()
        };
        if let Some(held) = held {
            held.notified().await;
        }
        let mut capacities = management.capacity.lock().unwrap();
        if ensure.lease != *management.lease.borrow() {
            return respond(400, "application/json", FENCED.into());
        }
        let request = capacities.requests.entry(ensure.request_id.clone()).or_insert_with(|| Request {
            ensure: ensure.clone(),
            state: CapacityState::Provisioning,
            message: String::new(),
            torn_down: false,
        });
        assert_eq!(request.ensure, EnsureCapacityRequest { lease: request.ensure.lease, ..ensure });
        let capacity = Capacity {
            request_id: request.ensure.request_id.clone(),
            state: request.state.into(),
            machine_id: String::new(),
            message: request.message.clone(),
        };
        if capacities.lost > 0 {
            capacities.lost -= 1;
            return respond(503, "application/json", UNAVAILABLE.into());
        }
        EnsureCapacityResponse { capacity: Some(capacity) }.encode_to_vec()
    } else {
        let release = ReleaseCapacityRequest::decode(body).unwrap();
        let mut capacities = management.capacity.lock().unwrap();
        let refused = capacities.stale.take_if(|id| *id == release.request_id).is_some();
        if refused || release.lease != *management.lease.borrow() {
            return respond(400, "application/json", FENCED.into());
        }
        let state = capacities.requests.get_mut(&release.request_id).map_or(CapacityState::Released, |request| {
            if matches!(request.state, CapacityState::Provisioning | CapacityState::Ready | CapacityState::Failed) {
                request.state = if request.torn_down { CapacityState::Released } else { CapacityState::Releasing };
            }
            request.state
        });
        let capacity = Capacity { request_id: release.request_id, state: state.into(), ..Capacity::default() };
        ReleaseCapacityResponse { capacity: Some(capacity) }.encode_to_vec()
    };
    respond(200, "application/proto", capacity)
}

pub(super) fn respond(status: u16, content_type: &str, body: Vec<u8>) -> hyper::Response<Body> {
    let response = hyper::Response::builder().status(status).header("content-type", content_type);
    response.body(Full::new(body.into()).boxed()).unwrap()
}

impl Management {
    /// Waits until at least `count` `EnsureCapacity` calls arrived.
    async fn ensured(&self, count: usize) {
        let arrived = async {
            while self.ensures().len() < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), arrived).await.expect("the calls arrived");
    }

    fn ensures(&self) -> Vec<EnsureCapacityRequest> {
        self.capacity.lock().unwrap().ensures.clone()
    }

    /// Records request `id` as provisioning, with its machine already removed when `torn_down`.
    fn requested(&self, id: &str, torn_down: bool) {
        let ensure = EnsureCapacityRequest { request_id: id.into(), ..EnsureCapacityRequest::default() };
        let request = Request { ensure, state: CapacityState::Provisioning, message: String::new(), torn_down };
        self.capacity.lock().unwrap().requests.insert(id.into(), request);
    }

    fn state(&self, id: &str) -> CapacityState {
        self.capacity.lock().unwrap().requests[id].state
    }

    /// Provisions request `id` as the reconciler would, into `state`.
    fn provision(&self, id: &str, state: CapacityState, message: &str) {
        let mut capacities = self.capacity.lock().unwrap();
        let request = capacities.requests.get_mut(id).unwrap();
        (request.state, request.message) = (state, message.into());
    }

    /// Removes request `id`'s machine as the reconciler would.
    fn tear_down(&self, id: &str) {
        let mut capacities = self.capacity.lock().unwrap();
        let request = capacities.requests.get_mut(id).unwrap();
        request.torn_down = true;
        if request.state == CapacityState::Releasing {
            request.state = CapacityState::Released;
        }
    }
}

fn spec() -> LaunchSpec {
    LaunchSpec {
        core_endpoint: "http://10.0.0.2:4000".into(),
        environment: "env_test".into(),
        release_id: "release-1".into(),
        app: "lobby".into(),
        profile: "small".into(),
        player_address: None,
        memory_mib: 512,
    }
}

fn client(harness: &Harness) -> chunk_management::Client {
    harness.management_config().client()
}

/// A launcher whose calls carry `lease`, and the cell it reads that from.
fn launcher(harness: &Harness, lease: Lease) -> (watch::Sender<Lease>, Arc<ManagementLauncher>) {
    let cell = watch::Sender::new(lease);
    let launcher = Arc::new(ManagementLauncher::new(client(harness), cell.subscribe()));
    (cell, launcher)
}

fn launch(
    launcher: &Arc<ManagementLauncher>,
    id: &str,
    cancel: &CancellationToken,
) -> tokio::task::JoinHandle<std::io::Result<()>> {
    let (launcher, id, cancel) = (launcher.clone(), id.to_owned(), cancel.clone());
    tokio::spawn(async move { launcher.launch(&id, "credential", &spec(), &cancel).await })
}

async fn settled<T>(call: tokio::task::JoinHandle<T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), call).await.expect("the call settled").unwrap()
}

/// Records a launch on `host` that no JVM of this core runs, as a core that crashed leaves it.
async fn recorded(harness: &Harness, host: &str) {
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let launch = chunk_control::Launch {
        deployment: "dep_a".into(),
        release: "release-1".into(),
        app: "lobby".into(),
        profile: "small".into(),
        process_id: format!("process-{host}"),
        generation: 1,
        boot: None,
    };
    core.control().unwrap().record_launch(host, launch).unwrap();
    core.stop(|| {}).await.unwrap();
}

#[tokio::test]
async fn a_launch_polls_one_request_to_ready_and_retries_a_lost_reply_under_its_id() {
    let harness = Harness::new().await;
    harness.management.capacity.lock().unwrap().lost = 1;
    let (_lease, launcher) = launcher(&harness, Lease::Held(0));
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    // The lost reply, then two polls of a machine still provisioning.
    harness.management.ensured(3).await;
    assert!(!launching.is_finished());
    harness.management.provision("host-1", CapacityState::Ready, "");
    settled(launching).await.unwrap();

    let expected = EnsureCapacityRequest {
        request_id: "host-1".into(),
        workload: Workload::Jvm.into(),
        machine_profile: "small".into(),
        release_id: "release-1".into(),
        app_id: "lobby".into(),
        lease: 0,
        credential: "credential".into(),
    };
    let ensures = harness.management.ensures();
    assert!(ensures.iter().all(|ensure| *ensure == expected), "{ensures:?}");
    assert_eq!(harness.management.capacity.lock().unwrap().requests.len(), 1);
}

#[tokio::test]
async fn a_poll_rejected_for_a_stale_lease_retries_under_the_next_one_whichever_arrives_first() {
    let harness = Harness::new().await;
    let (lease, launcher) = launcher(&harness, Lease::Held(0));
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    harness.management.ensured(1).await;

    // Management grants a re-attach lease, and rejects the next poll before core learns that lease.
    harness.management.lease.send_replace(1);
    let rejected = harness.management.ensures().len() + 1;
    harness.management.ensured(rejected).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(harness.management.ensures().len(), rejected, "polled again under the stale lease");
    lease.send_replace(Lease::Held(1));
    harness.management.ensured(rejected + 1).await;
    assert_eq!(harness.management.ensures()[rejected].lease, 1);
    assert!(!launching.is_finished());

    // Core learns the next lease while management still handles a poll under the previous one.
    let held = Arc::new(Notify::new());
    harness.management.capacity.lock().unwrap().held = Some(held.clone());
    let polled = harness.management.ensures().len() + 1;
    harness.management.ensured(polled).await;
    harness.management.lease.send_replace(2);
    lease.send_replace(Lease::Held(2));
    held.notify_one();
    harness.management.ensured(polled + 1).await;
    assert_eq!(harness.management.ensures()[polled].lease, 2);

    harness.management.provision("host-1", CapacityState::Ready, "");
    settled(launching).await.unwrap();
}

#[tokio::test]
async fn a_failed_request_is_an_error_and_a_superseded_core_launches_nothing_and_releases_at_once() {
    let harness = Harness::new().await;
    let (lease, launcher) = launcher(&harness, Lease::Held(0));
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    harness.management.ensured(1).await;
    harness.management.provision("host-1", CapacityState::Failed, "no room");
    let error = settled(launching).await.unwrap_err();
    assert!(error.to_string().contains("no room"), "{error}");

    // Management releases a superseded core's requests itself.
    harness.management.lease.send_replace(1);
    lease.send_replace(Lease::Superseded);
    let ensured = harness.management.ensures().len();
    assert!(settled(launch(&launcher, "host-2", &CancellationToken::new())).await.is_err());
    assert!(launcher.release("host-1").await.unwrap());
    assert_eq!(harness.management.ensures().len(), ensured);
    assert_eq!(harness.management.state("host-1"), CapacityState::Failed);
}

#[tokio::test]
async fn a_cancelled_launch_returns_and_its_release_confirms_only_once_the_machine_is_gone() {
    let harness = Harness::new().await;
    let (_lease, launcher) = launcher(&harness, Lease::Held(0));
    let cancel = CancellationToken::new();
    let launching = launch(&launcher, "host-1", &cancel);
    harness.management.ensured(1).await;
    cancel.cancel();
    assert!(settled(launching).await.is_err());

    assert!(!launcher.release("host-1").await.unwrap());
    harness.management.tear_down("host-1");
    assert!(launcher.release("host-1").await.unwrap());
    // Released for good, the request never provisions again.
    assert!(settled(launch(&launcher, "host-1", &CancellationToken::new())).await.is_err());
    assert!(launcher.release("never-launched").await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_waits_for_the_first_attach_and_a_release_keeps_its_lease_after_it_ends() {
    let harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let (lease, launcher) = launcher(&harness, Lease::Waiting);
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(harness.management.ensures().is_empty());

    let core = Core::start(harness.core(), || {}).await.unwrap();
    let gateway = OnceLock::new();
    let managed =
        Managed::new(&harness.management_config(), lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let ready = async {
        harness.management.ensured(1).await;
        harness.management.provision("host-1", CapacityState::Ready, "");
        settled(launching).await.unwrap();
    };
    tokio::select! {
        error = managed.run() => panic!("management stopped: {error}"),
        () = ready => {}
    }
    assert_eq!(harness.management.ensures()[0].lease, 1);

    // The attach ended with `managed`, and the release still carries its lease.
    assert!(!launcher.release("host-1").await.unwrap());
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_releases_machines_through_management_and_a_fenced_core_still_stops() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    recorded(&harness, "remote-1").await;
    harness.management.requested("remote-1", true);
    let (stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.expect(1, "dep_a", DeploymentState::Active).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert_eq!(harness.management.state("remote-1"), CapacityState::Released);

    // A machine management never confirms released: once fenced, core leaves it to management and stops.
    recorded(&harness, "remote-2").await;
    harness.management.requested("remote-2", false);
    let (_stop, running) = harness.start();
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.management.lease.send_modify(|lease| *lease += 1);
    let error = tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("fenced"), "{error}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_leaves_unconfirmed_releases_to_management_once_bounded_and_activates_nothing_meanwhile() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.stalled());
    recorded(&harness, "remote-1").await;
    recorded(&harness, "remote-2").await;
    // remote-1 stays releasing, and remote-2's release waits for a lease that never comes.
    harness.management.requested("remote-1", false);
    harness.management.capacity.lock().unwrap().stale = Some("remote-2".into());
    let (stop, running) = harness.start_bounded(Duration::from_secs(2));
    harness.expect(1, "dep_a", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    assert!(harness.management.capacity.lock().unwrap().stale.is_some());
    stop.cancel();
    let refused = async {
        while harness.management.capacity.lock().unwrap().stale.is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), refused).await.expect("remote-2's release was refused");

    // Desired once shutdown began, dep_b neither starts nor activates.
    harness.deploy("dep_b", harness.valid());
    tokio::time::timeout(Duration::from_secs(30), running).await.unwrap().unwrap().unwrap();
    assert_eq!(harness.management.state("remote-1"), CapacityState::Releasing);
    assert!(std::iter::from_fn(|| harness.reported.try_recv().ok()).all(|report| report.deployment.is_none()));
}
