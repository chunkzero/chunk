//! The management launcher against the fake management's capacity requests.

use super::*;
use crate::{
    Core, LaunchSpec, Launcher,
    managed::{Managed, ManagementLauncher},
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
}

struct Request {
    ensure: EnsureCapacityRequest,
    state: CapacityState,
    message: String,
    /// Whether the reconciler removed the machine.
    torn_down: bool,
}

/// Answers an `EnsureCapacity` or `ReleaseCapacity` call.
pub(super) fn serve(management: &Management, path: &str, body: &[u8]) -> hyper::Response<Body> {
    let lease = *management.lease.borrow();
    let mut capacities = management.capacity.lock().unwrap();
    let capacity = if path.ends_with("/EnsureCapacity") {
        let ensure = EnsureCapacityRequest::decode(body).unwrap();
        capacities.ensures.push(ensure.clone());
        if ensure.lease != lease {
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
        if release.lease != lease {
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

fn respond(status: u16, content_type: &str, body: Vec<u8>) -> hyper::Response<Body> {
    let response = hyper::Response::builder().status(status).header("content-type", content_type);
    response.body(Full::new(body.into()).boxed()).unwrap()
}

impl Management {
    /// Waits until at least `count` `EnsureCapacity` calls arrived.
    async fn ensured(&self, count: usize) {
        let arrived = async {
            while self.capacity.lock().unwrap().ensures.len() < count {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), arrived).await.expect("the calls arrived");
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
    ManagementConfig { url: harness.url.clone(), token: "secret".into() }.client()
}

/// A launcher whose calls carry `lease`, and the cell it reads that from.
fn launcher(harness: &Harness, lease: Option<u64>) -> (watch::Sender<Option<u64>>, Arc<ManagementLauncher>) {
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

#[tokio::test]
async fn a_launch_polls_one_request_to_ready_and_retries_a_lost_reply_under_its_id() {
    let harness = Harness::new().await;
    harness.management.capacity.lock().unwrap().lost = 1;
    let (_lease, launcher) = launcher(&harness, Some(0));
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    // The lost reply, then two polls of a machine still provisioning.
    harness.management.ensured(3).await;
    assert!(!launching.is_finished());
    harness.management.provision("host-1", CapacityState::Ready, "");
    settled(launching).await.unwrap();

    let capacities = harness.management.capacity.lock().unwrap();
    let expected = EnsureCapacityRequest {
        request_id: "host-1".into(),
        workload: Workload::Jvm.into(),
        machine_profile: "small".into(),
        release_id: "release-1".into(),
        app_id: "lobby".into(),
        lease: 0,
        credential: "credential".into(),
    };
    assert!(capacities.ensures.iter().all(|ensure| *ensure == expected), "{:?}", capacities.ensures);
    assert_eq!(capacities.requests.len(), 1);
}

#[tokio::test]
async fn a_failed_request_or_a_fenced_lease_is_an_error() {
    let harness = Harness::new().await;
    let (_lease, launcher) = launcher(&harness, Some(0));
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    harness.management.ensured(1).await;
    harness.management.provision("host-1", CapacityState::Failed, "no room");
    let error = settled(launching).await.unwrap_err();
    assert!(error.to_string().contains("no room"), "{error}");

    // A newer core attached.
    harness.management.lease.send_replace(1);
    let error = settled(launch(&launcher, "host-2", &CancellationToken::new())).await.unwrap_err();
    assert!(error.to_string().contains("failed_precondition"), "{error}");
    assert!(launcher.release("host-1").await.is_err());
}

#[tokio::test]
async fn a_cancelled_launch_returns_and_its_release_confirms_only_once_the_machine_is_gone() {
    let harness = Harness::new().await;
    let (_lease, launcher) = launcher(&harness, Some(0));
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
    let (lease, launcher) = launcher(&harness, None);
    let launching = launch(&launcher, "host-1", &CancellationToken::new());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(harness.management.capacity.lock().unwrap().ensures.is_empty());

    let core = Core::start(harness.core(), || {}).await.unwrap();
    let gateway = OnceLock::new();
    let managed = Managed::new(client(&harness), lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let ready = async {
        harness.management.ensured(1).await;
        harness.management.provision("host-1", CapacityState::Ready, "");
        settled(launching).await.unwrap();
    };
    tokio::select! {
        error = managed.run() => panic!("management stopped: {error}"),
        () = ready => {}
    }
    assert_eq!(harness.management.capacity.lock().unwrap().ensures[0].lease, 1);

    // The attach ended with `managed`, and the release still carries its lease.
    assert!(!launcher.release("host-1").await.unwrap());
    core.stop(|| {}).await.unwrap();
}
