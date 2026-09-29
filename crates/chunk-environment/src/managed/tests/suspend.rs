//! Idle reports and wake alarms against the fake management.

use super::status::{advance_for, hold_time};
use super::{launcher::respond, *};
use crate::{
    Core,
    managed::{Lease, Managed, alarm::Alarm},
};
use chunk_management::v1::{SetWakeAlarmRequest, SetWakeAlarmResponse};
use std::{
    pin::Pin,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::time::Instant;

/// Management's stored wake alarm, kept by the rules of `packages/management`.
#[derive(Default)]
pub(super) struct Alarms {
    /// Every `SetWakeAlarm` call, in order.
    calls: Vec<SetWakeAlarmRequest>,
    stored: SetWakeAlarmResponse,
    /// Holds the next call, once recorded, until notified; management then handles it.
    held: Option<Arc<Notify>>,
}

/// Answers a `SetWakeAlarm` call.
pub(super) async fn serve(management: &Management, body: &[u8]) -> hyper::Response<Body> {
    let request = SetWakeAlarmRequest::decode(body).unwrap();
    let held = {
        let mut alarms = management.alarm.lock().unwrap();
        alarms.calls.push(request);
        alarms.held.take()
    };
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

    // A wake's new revision invalidates the report at once, and the grace period starts over under it.
    harness.management.publish(&mut harness.management.records.lock().unwrap());
    assert!(until_ready(&mut running, reported, 2, false).await <= SLACK);
    assert!(until_ready(&mut running, reported, 2, true).await + SLACK >= GRACE);

    drop((running, time));
    tokio::time::resume();
    core.stop(|| {}).await.unwrap();
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
