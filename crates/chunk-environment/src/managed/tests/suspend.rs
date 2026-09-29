//! Idle reports and wake alarms against the fake management.

use super::status::{advance_for, hold_time};
use super::{launcher::respond, *};
use crate::{
    Core, Gateway, PlatformTarget,
    managed::{Lease, Managed, READ_WAIT, alarm::Alarm, read_within},
};
use chunk_management::v1::{SetWakeAlarmRequest, SetWakeAlarmResponse};
use chunk_proto::sync::v1::{ActiveArguments, GatewayArguments, SubscribeRequest, Update};
use std::{
    net::SocketAddr,
    pin::Pin,
    sync::OnceLock,
    task::Poll,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::{TcpListener, TcpStream},
    time::Instant,
};
use tonic::Streaming;

/// Management's stored wake alarm, kept by the rules of `packages/management`.
#[derive(Default)]
pub(super) struct Alarms {
    /// Every `SetWakeAlarm` call, in order.
    calls: Vec<SetWakeAlarmRequest>,
    stored: SetWakeAlarmResponse,
    /// Holds the next call, once recorded, until notified; management then handles it.
    held: Option<Arc<Notify>>,
    /// While set, every call is answered with this alarm, and none is stored.
    echo: Option<SetWakeAlarmResponse>,
}

/// Answers a `SetWakeAlarm` call.
pub(super) async fn serve(management: &Management, body: &[u8]) -> hyper::Response<Body> {
    let request = SetWakeAlarmRequest::decode(body).unwrap();
    let (held, echo) = {
        let mut alarms = management.alarm.lock().unwrap();
        alarms.calls.push(request);
        (alarms.held.take(), alarms.echo)
    };
    if let Some(echo) = echo {
        return respond(200, "application/proto", echo.encode_to_vec());
    }
    if let Some(held) = held {
        held.notified().await;
    }
    if request.lease < *management.lease.borrow() {
        return respond(400, "application/json", FENCED.into());
    }
    let mut alarms = management.alarm.lock().unwrap();
    let stored = &mut alarms.stored;
    let order = (request.epoch, request.generation).cmp(&(stored.epoch, stored.generation));
    if order.is_gt() {
        *stored =
            SetWakeAlarmResponse { generation: request.generation, due_time: request.due_time, epoch: request.epoch };
    } else if order.is_eq() && stored.due_time != request.due_time {
        return respond(400, "application/json", FENCED.into());
    }
    respond(200, "application/proto", stored.encode_to_vec())
}

const GRACE: Duration = Duration::from_secs(30);
/// How far from a due time a report may be observed, at one-second observations.
const SLACK: Duration = Duration::from_secs(2);

/// Advances held time a second at a time until management applies a report under `revision` whose readiness to suspend
/// is `ready`, and returns how long that took.
async fn until_ready(
    running: &mut Pin<Box<impl Future<Output = std::io::Error>>>,
    reported: &mut mpsc::UnboundedReceiver<ReportStatusRequest>,
    revision: u64,
    ready: bool,
) -> Duration {
    let start = Instant::now();
    loop {
        while let Ok(report) = reported.try_recv() {
            if (report.desired_revision, report.ready_to_suspend) == (revision, ready) {
                return start.elapsed();
            }
        }
        assert!(start.elapsed() < GRACE * 2, "no report under revision {revision} was ready: {ready}");
        tokio::select! {
            error = running.as_mut() => panic!("{error}"),
            () = advance_for(Duration::from_secs(1)) => {}
        }
    }
}

/// Advances held time a second at a time for `limit`, returning the reports management applied meanwhile.
async fn reports_for(
    running: &mut Pin<Box<impl Future<Output = std::io::Error>>>,
    reported: &mut mpsc::UnboundedReceiver<ReportStatusRequest>,
    limit: Duration,
) -> Vec<ReportStatusRequest> {
    let mut reports = Vec::new();
    for _ in 0..limit.as_secs() {
        tokio::select! {
            error = running.as_mut() => panic!("{error}"),
            () = advance_for(Duration::from_secs(1)) => {}
        }
        reports.extend(std::iter::from_fn(|| reported.try_recv().ok()));
    }
    reports
}

#[tokio::test]
async fn core_is_ready_to_suspend_only_after_the_grace_period_and_a_login_or_a_wake_ends_it_at_once() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let config = ManagementConfig { suspend_after: Some(GRACE), ..harness.management_config() };
    let listener = GatewayConfig::new("127.0.0.1:0".parse().unwrap());
    let managed = Managed::new(&config, lease, "env_test".into(), &harness.state(), &core, &gateway, Some(listener));
    let mut running = Box::pin(managed.run());
    let deployed = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await
    };
    tokio::select! {
        error = &mut running => panic!("{error}"),
        report = deployed => assert!(!report.ready_to_suspend),
    }

    tokio::time::pause();
    let time = hold_time();
    let reported = &mut harness.reported;
    let idle = until_ready(&mut running, reported, 1, true).await;
    assert!(idle + SLACK >= GRACE && idle <= GRACE + SLACK, "{idle:?}");
    let stored = harness.management.alarm.lock().unwrap().stored;
    assert_eq!((stored.epoch, stored.due_time), (core.epoch().unwrap(), None));

    // A login ends it within a second, and once the player leaves, the grace period starts over.
    let login = tokio::net::TcpStream::connect(gateway.get().unwrap().address()).await.unwrap();
    assert!(until_ready(&mut running, reported, 1, false).await <= SLACK);
    drop(login);
    assert!(until_ready(&mut running, reported, 1, true).await + SLACK >= GRACE);

    // So does backend work, which holds it off however long it runs; the grace period starts over once it ends.
    let work = core.backend().unwrap().activity().begin();
    assert!(until_ready(&mut running, reported, 1, false).await <= SLACK);
    tokio::select! {
        error = running.as_mut() => panic!("{error}"),
        () = advance_for(GRACE + SLACK) => {}
    }
    drop(work);
    assert!(until_ready(&mut running, reported, 1, true).await + SLACK >= GRACE);

    // A wake's new revision invalidates the report at once, and the grace period starts over under it.
    harness.management.publish(&mut harness.management.records.lock().unwrap());
    assert!(until_ready(&mut running, reported, 2, false).await <= SLACK);
    assert!(until_ready(&mut running, reported, 2, true).await + SLACK >= GRACE);

    drop((running, time));
    tokio::time::resume();
    core.stop(|| {}).await.unwrap();
}

/// A gateway on its own machine, which follows its topic and reports its connections over the sync protocol.
struct RemoteGateway {
    client: CoreClient<tonic::transport::Channel>,
    id: String,
    credential: String,
}

impl RemoteGateway {
    /// Follows the gateway's topic as process `remote`, returning the stream and its ID.
    async fn follow(&mut self) -> (Streaming<Update>, String) {
        let subscription = SubscribeRequest {
            topic: format!("gateway/{}", self.id),
            arguments: GatewayArguments { instance: "remote".into() }.encode_to_vec(),
            ..SubscribeRequest::default()
        };
        let mut updates = self.client.subscribe(self.authorized(subscription)).await.unwrap().into_inner();
        let stream = updates.message().await.unwrap().unwrap().stream;
        (updates, stream)
    }

    fn authorized<T>(&self, message: T) -> tonic::Request<T> {
        let mut request = tonic::Request::new(message);
        let bearer = format!("Bearer {}", self.credential).parse().unwrap();
        request.metadata_mut().insert("authorization", bearer);
        request
    }

    /// Reports `connections` on `stream`, returning the error core answered with, if any.
    async fn active(&mut self, stream: &str, connections: u32) -> Option<Code> {
        let message = CallRequest {
            method: "chunk:active".into(),
            arguments: ActiveArguments { connections }.encode_to_vec(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        match self.client.call(self.authorized(message)).await.unwrap().into_inner().outcome {
            Some(Outcome::Error(error)) => Some(error.code()),
            _ => None,
        }
    }

    /// Reports `connections` on `stream` every second for `limit`, returning whether each report management applied
    /// meanwhile was ready to suspend.
    async fn report_for(
        &mut self,
        running: &mut Pin<Box<impl Future<Output = std::io::Error>>>,
        reported: &mut mpsc::UnboundedReceiver<ReportStatusRequest>,
        stream: &str,
        connections: u32,
        limit: Duration,
    ) -> Vec<bool> {
        let mut ready = Vec::new();
        for _ in 0..limit.as_secs() {
            self.active(stream, connections).await;
            tokio::select! {
                error = running.as_mut() => panic!("{error}"),
                () = advance_for(Duration::from_secs(1)) => {}
            }
            ready.extend(std::iter::from_fn(|| reported.try_recv().ok()).map(|report| report.ready_to_suspend));
        }
        ready
    }
}

#[tokio::test]
async fn a_gateway_core_can_t_hear_from_keeps_it_awake_until_a_new_stream_reports_or_its_machine_is_revoked() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let config = ManagementConfig { suspend_after: Some(GRACE), ..harness.management_config() };
    let managed = Managed::new(&config, lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let mut running = Box::pin(managed.run());
    let deployed = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await
    };
    tokio::select! {
        error = &mut running => panic!("{error}"),
        _ = deployed => {}
    }
    let client = CoreClient::connect(core.target().unwrap().core).await.unwrap();
    let credential = core.gateway_credential("remote").unwrap();
    let mut remote = RemoteGateway { client, id: "remote".into(), credential };
    let (updates, stream) = remote.follow().await;

    tokio::time::pause();
    let time = hold_time();
    let reported = &mut harness.reported;
    // Heard reporting no connections, the gateway lets core become ready.
    let ready = remote.report_for(&mut running, reported, &stream, 0, GRACE + SLACK * 2).await;
    assert_eq!(ready.last(), Some(&true), "{ready:?}");

    // Then a player connects, but every report of it fails: core stops hearing from the gateway, which counts as
    // active from then on, however long the grace period passes.
    let ready = remote.report_for(&mut running, reported, "unknown", 1, GRACE * 2).await;
    let revoked = ready.iter().position(|ready| !ready).expect("readiness was revoked");
    assert!(revoked <= 5 && !ready[revoked..].contains(&true), "{ready:?}");
    assert_eq!(remote.active("unknown", 1).await, Some(Code::Stopped));

    // Its stream ends too, and the gateway may still hold the player, so it keeps counting.
    drop(updates);
    let ready = remote.report_for(&mut running, reported, &stream, 0, GRACE * 2).await;
    assert!(!ready.contains(&true), "{ready:?}");

    // A new stream's report decides: none, and the grace period runs out.
    let (updates, stream) = remote.follow().await;
    let ready = remote.report_for(&mut running, reported, &stream, 0, GRACE + SLACK * 2).await;
    assert_eq!(ready.last(), Some(&true), "{ready:?}");

    // A player keeps it awake again until the gateway's machine is revoked, after which it no longer counts.
    let ready = remote.report_for(&mut running, reported, &stream, 1, SLACK * 2).await;
    assert_eq!(ready.last(), Some(&false), "{ready:?}");
    core.revoke_gateway("remote").unwrap();
    drop(updates);
    assert!(until_ready(&mut running, reported, 1, true).await + SLACK >= GRACE);

    drop((running, time));
    tokio::time::resume();
    core.stop(|| {}).await.unwrap();
}

/// Relays connections to `target` while `open` holds true. Once it's false, relayed connections are cut and new ones
/// refused.
async fn relay(target: SocketAddr, open: watch::Receiver<bool>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut inbound, _) = listener.accept().await.unwrap();
            let mut open = open.clone();
            if !*open.borrow() {
                continue;
            }
            tokio::spawn(async move {
                let Ok(mut outbound) = TcpStream::connect(target).await else { return };
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound) => {}
                    _ = open.wait_for(|open| !open) => {}
                }
            });
        }
    });
    address
}

#[tokio::test]
async fn a_gateway_cut_off_from_core_keeps_it_awake_for_the_player_it_still_holds() {
    const SHORT: Duration = Duration::from_secs(2);
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let config = ManagementConfig { suspend_after: Some(SHORT), ..harness.management_config() };
    let managed = Managed::new(&config, lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let mut running = Box::pin(managed.run());
    let deployed = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await
    };
    tokio::select! {
        error = &mut running => panic!("{error}"),
        _ = deployed => {}
    }
    // The gateway reaches core through a relay the test can cut.
    let target = core.target().unwrap();
    let (open, opened) = watch::channel(true);
    let relayed = relay(target.core.strip_prefix("http://").unwrap().parse().unwrap(), opened).await;
    let target = PlatformTarget { core: format!("http://{relayed}"), deployment: "dep_a".into(), ..target };
    let listener = Gateway::start(GatewayConfig::new("127.0.0.1:0".parse().unwrap()), target).await.unwrap();

    tokio::time::pause();
    let time = hold_time();
    let reported = &mut harness.reported;
    until_ready(&mut running, reported, 1, true).await;

    // The gateway's stream breaks, and while it's cut off a player connects, which core can't hear of. However long
    // the grace period passes, core stays awake for the player the gateway may hold.
    open.send_replace(false);
    let player = TcpStream::connect(listener.address()).await.unwrap();
    assert!(until_ready(&mut running, reported, 1, false).await <= SLACK);
    let reports = reports_for(&mut running, reported, SHORT * 2).await;
    assert!(reports.iter().all(|report| !report.ready_to_suspend));

    // Once the gateway follows core again, its reports decide: the player is still there, then leaves.
    open.send_replace(true);
    let reports = reports_for(&mut running, reported, SLACK).await;
    assert!(reports.iter().all(|report| !report.ready_to_suspend));
    drop(player);
    assert!(until_ready(&mut running, reported, 1, true).await + SLACK >= SHORT);

    drop((running, time));
    tokio::time::resume();
    listener.stop().await.unwrap();
    core.stop(|| {}).await.unwrap();
}

#[tokio::test]
async fn a_held_address_lookup_keeps_core_awake_without_holding_up_its_reports() {
    let mut harness = Harness::new().await;
    harness.deploy("dep_a", harness.valid());
    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let config = ManagementConfig { suspend_after: Some(GRACE), ..harness.management_config() };
    let managed = Managed::new(&config, lease, "env_test".into(), &harness.state(), &core, &gateway, None);
    let mut running = Box::pin(managed.run());
    let deployed = async {
        harness.expect(1, "dep_a", DeploymentState::InProgress).await;
        harness.expect(1, "dep_a", DeploymentState::Active).await
    };
    tokio::select! {
        error = &mut running => panic!("{error}"),
        _ = deployed => {}
    }

    tokio::time::pause();
    let time = hold_time();
    let reported = &mut harness.reported;
    until_ready(&mut running, reported, 1, true).await;

    // A gateway on every interface starts while the lookup of the address that reaches management is held.
    let mut held =
        Box::pin(managed.private_address.management.get_or_try_init(std::future::pending::<std::io::Result<_>>));
    std::future::poll_fn(|context| {
        assert!(held.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    let target = PlatformTarget { deployment: "dep_a".into(), ..core.target().unwrap() };
    let started = Gateway::start(GatewayConfig::new("0.0.0.0:0".parse().unwrap()), target).await.unwrap();
    assert!(gateway.set(started).is_ok());

    // Each observation gives up on the lookup: core isn't ready however long nothing happens, and reports keep going
    // out without an address management hasn't accepted.
    let reports = reports_for(&mut running, reported, GRACE + SLACK * 2).await;
    assert!(reports.len() >= 2, "{} reports", reports.len());
    assert!(reports.iter().all(|report| !report.ready_to_suspend && report.gateway_addresses.is_empty()));

    // Once the lookup answers, the gateway's address is reported, and core is ready after a full grace period.
    drop(held);
    let address = format!("127.0.0.1:{}", gateway.get().unwrap().address().port());
    let reports = reports_for(&mut running, reported, SLACK).await;
    assert!(reports.iter().any(|report| report.gateway_addresses == [address.clone()]), "{address}");
    assert!(until_ready(&mut running, reported, 1, true).await + SLACK * 2 >= GRACE);

    drop((running, time));
    tokio::time::resume();
    core.stop(|| {}).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_signal_read_that_is_held_counts_as_unknown_once_its_wait_runs_out() {
    let (answer, held) = tokio::sync::oneshot::channel::<u64>();
    let start = Instant::now();
    assert_eq!(read_within(held).await, None);
    assert_eq!(start.elapsed(), READ_WAIT);
    drop(answer);
    let (answer, answered) = tokio::sync::oneshot::channel::<u64>();
    answer.send(1).unwrap();
    assert_eq!(read_within(answered).await, Some(1));
}

/// A backend whose `schedule` mutation schedules a job at the Unix millisecond it's given.
async fn scheduling_backend(directory: &Path) -> chunk_backend::Backend {
    let store = chunk_store::SqliteStore::open(directory.join("alarm.sqlite"), "env_test").unwrap();
    let backend = chunk_backend::Backend::new("env_test".into(), Box::new(store)).unwrap();
    let function = |kind, visibility, arguments, result| chunk_contract::Function {
        kind,
        visibility,
        export: String::new(),
        arguments,
        result,
    };
    let (schedule, job) = (
        function(
            chunk_contract::FunctionKind::Mutation,
            chunk_contract::Visibility::Public,
            chunk_contract::Schema::Integer,
            chunk_contract::Schema::String,
        ),
        function(
            chunk_contract::FunctionKind::Action,
            chunk_contract::Visibility::Internal,
            chunk_contract::Schema::Null,
            chunk_contract::Schema::Null,
        ),
    );
    let deployment = chunk_contract::Deployment {
        contracts: chunk_contract::Contracts::default(),
        contract_version: 2,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: "dep_jobs".into(),
        source: "export function schedule(ctx, at) { return ctx.scheduler.runAt(at, 'job', null); }\n\
                 export async function job() { return null; }"
            .into(),
        tables: BTreeMap::new(),
        functions: BTreeMap::from([
            ("schedule".into(), chunk_contract::Function { export: "schedule".into(), ..schedule }),
            ("job".into(), chunk_contract::Function { export: "job".into(), ..job }),
        ]),
    };
    backend.deploy(deployment).await.unwrap();
    backend
}

/// Schedules a job due `at`, and returns the backend's alarm generation after it.
async fn schedule(backend: &chunk_backend::Backend, at: i64) -> u64 {
    let call = chunk_backend::Call {
        deployment: chunk_backend::DeploymentId::new("dep_jobs").unwrap(),
        function: "schedule".into(),
        arguments: serde_json::json!(at).into(),
        caller: serde_json::json!({ "player": "alice" }).into(),
    };
    backend.mutate(uuid::Uuid::new_v4().to_string(), call).await.unwrap();
    backend.wake_handoff().await.unwrap().generation
}

async fn eventually(mut done: impl AsyncFnMut() -> bool) {
    let waited = async {
        while !done().await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), waited).await.expect("the condition held");
}

fn due(millis: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(u64::try_from(millis).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_lease_or_generation_is_read_again_before_the_alarm_is_acknowledged() {
    let harness = Harness::new().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = scheduling_backend(directory.path()).await;
    let epoch = backend.system().epoch().0;
    let at = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap() + 3_600_000;
    let first = schedule(&backend, at).await;
    // Management granted lease 2, which core hasn't seen yet.
    harness.management.lease.send_replace(2);
    let held = Mutex::new(1);
    let alarm = Alarm::new(harness.management_config().client());
    let handing = alarm.keep_handing_off(&backend, epoch, || Some(*held.lock().unwrap()));
    let alarms = &harness.management.alarm;
    let calls = || alarms.lock().unwrap().calls.iter().map(|call| (call.lease, call.generation)).collect::<Vec<_>>();
    let handed = async |generation| {
        let handoff = backend.wake_handoff().await.unwrap();
        handoff.generation == generation && handoff.acknowledged && alarm.settled(epoch, &handoff)
    };
    let checks = async {
        eventually(async || calls().len() >= 2).await;
        assert!(!handed(first).await);
        assert_eq!(alarms.lock().unwrap().stored, SetWakeAlarmResponse::default());

        // Under the lease core holds now, the alarm is stored, then acknowledged.
        *held.lock().unwrap() = 2;
        eventually(async || handed(first).await).await;
        assert!(calls().iter().all(|&(lease, generation)| generation == first && (lease == 1 || lease == 2)));
        let stored = alarms.lock().unwrap().stored;
        assert_eq!((stored.epoch, stored.generation, stored.due_time), (epoch, first, Some(due(at).into())));

        // The backend's alarm moves on while management stores the one before, so that one is never acknowledged, and
        // the next is read and handed off.
        let release = Arc::new(Notify::new());
        alarms.lock().unwrap().held = Some(release.clone());
        let second = schedule(&backend, at - 60_000).await;
        eventually(async || calls().last() == Some(&(2, second))).await;
        let third = schedule(&backend, at - 120_000).await;
        release.notify_one();
        eventually(async || handed(third).await).await;
        let stored = alarms.lock().unwrap().stored;
        assert_eq!((stored.generation, stored.due_time), (third, Some(due(at - 120_000).into())));
        assert_eq!(calls().last(), Some(&(2, third)));
    };
    tokio::select! {
        never = handing => match never {},
        () = checks => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_an_exact_echo_acknowledges_an_alarm_and_a_restored_acknowledgement_is_handed_off_again() {
    let harness = Harness::new().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = scheduling_backend(directory.path()).await;
    let epoch = backend.system().epoch().0;
    let at = i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap() + 3_600_000;
    let generation = schedule(&backend, at).await;
    let lease = *harness.management.lease.borrow();
    let alarms = &harness.management.alarm;
    let calls = || alarms.lock().unwrap().calls.clone();
    let (alarm, restored) =
        (Alarm::new(harness.management_config().client()), Alarm::new(harness.management_config().client()));
    let settled = async |alarm: &Alarm, epoch| alarm.settled(epoch, &backend.wake_handoff().await.unwrap());
    let checks = async {
        // Management answers with an alarm that differs in its epoch, generation or due time, which is never acknowledged.
        let exact = SetWakeAlarmResponse { generation, due_time: Some(due(at).into()), epoch };
        for echo in [
            SetWakeAlarmResponse { epoch: epoch + 1, ..exact },
            SetWakeAlarmResponse { generation: generation + 1, ..exact },
            SetWakeAlarmResponse { due_time: Some(due(at + 1).into()), ..exact },
        ] {
            alarms.lock().unwrap().echo = Some(echo);
            let before = calls().len();
            eventually(async || calls().len() >= before + 2).await;
            assert!(!backend.wake_handoff().await.unwrap().acknowledged);
            assert!(!settled(&alarm, epoch).await);
        }
        alarms.lock().unwrap().echo = None;
        eventually(async || settled(&alarm, epoch).await).await;

        // Restored into an epoch management hasn't seen, the backend still says its alarm was acknowledged, but a new
        // process hands it off under that epoch anyway.
        let restored_epoch = epoch + 1;
        assert!(backend.wake_handoff().await.unwrap().acknowledged && !settled(&restored, restored_epoch).await);
        let handed = async {
            eventually(async || settled(&restored, restored_epoch).await).await;
        };
        tokio::select! {
            never = restored.keep_handing_off(&backend, restored_epoch, || Some(lease)) => match never {},
            () = handed => {}
        }
        let last = calls().last().copied().unwrap();
        assert_eq!((last.epoch, last.generation, last.due_time), (restored_epoch, generation, Some(due(at).into())));
        assert_eq!(alarms.lock().unwrap().stored.epoch, restored_epoch);
    };
    tokio::select! {
        never = alarm.keep_handing_off(&backend, epoch, || Some(lease)) => match never {},
        () = checks => {}
    }
}
