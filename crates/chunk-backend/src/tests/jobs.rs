use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::DeploymentId;
use chunk_store::{Job, JobState, SqliteStore, Storage};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{Backend, Call, Error};

pub(super) fn now() -> i64 {
    i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}
fn backend(directory: &tempfile::TempDir) -> Backend {
    let store = SqliteStore::open(directory.path().join("jobs.db"), "jobs").unwrap();
    let effects = crate::ActionEffects::new("jobs".into()).unwrap();
    Backend::with_action_bytes("jobs".into(), Box::new(store), effects, crate::limits::ACTION_BYTES).unwrap()
}
fn call(version: &str, function: &str, player: &str, arguments: serde_json::Value) -> Call {
    Call {
        deployment: DeploymentId::new(version).unwrap(),
        function: function.into(),
        arguments: arguments.into(),
        caller: json!({"player":player}).into(),
    }
}
pub(super) fn deployment(id: &str, increment: i32) -> Deployment {
    let schedule: Schema = serde_json::from_value(
        json!({"type":"object","fields":{"at":{"schema":{"type":"integer"}},"delay":{"schema":{"type":"integer"}}}}),
    )
    .unwrap();
    let retry: Schema = serde_json::from_value(json!({"type":"object","fields":{"at":{"schema":{"type":"integer"}},"id":{"schema":{"type":"string"}},"ack":{"schema":{"type":"boolean"}}}})).unwrap();
    Deployment {
        contracts: Contracts::default(),
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: format!(
            r"
export function read(ctx) {{ return ctx.db.get('counts',ctx.caller.player)?.value ?? 0; }}
export function increment(ctx) {{ const value=read(ctx)+{increment}; ctx.db.put('counts',ctx.caller.player,{{value}}); return value; }}
export function foreground(ctx) {{ return increment(ctx); }}
export function schedule(ctx,args) {{ ctx.db.put('counts',ctx.caller.player,{{value:40}}); return ctx.scheduler.runAt(args.at,'flow',args.delay); }}
export function rollback(ctx,args) {{ schedule(ctx,args); throw Error('abort'); }}
export function badTarget(ctx,args) {{ ctx.db.put('counts',ctx.caller.player,{{value:90}}); return ctx.scheduler.runAt(args.at,'read',null); }}
export function badArgs(ctx,args) {{ ctx.db.put('counts',ctx.caller.player,{{value:90}}); return ctx.scheduler.runAt(args.at,'flow','wrong'); }}
export function overflow(ctx,args) {{ for(let i=0;i<17;i++) schedule(ctx,args); return 'bad'; }}
export function querySchedule(ctx,args) {{ return ctx.scheduler.runAt(args.at,'flow',args.delay); }}
export async function flow(ctx,delay) {{ await ctx.runMutation('increment',null); await ctx.sleep(delay); await ctx.runMutation('increment',null); return ctx.invocationId; }}
export function cancel(ctx,id) {{ ctx.scheduler.cancel(id); return null; }}
export function retry(ctx,args) {{ ctx.scheduler.retry(args.id,args.at,args.ack); return null; }}
"
        ),
        tables: serde_json::from_value(json!({"counts":{"fields":{"value":{"schema":{"type":"integer"}}}}})).unwrap(),
        functions: [
            ("read", FunctionKind::Query, Visibility::Public, Schema::Null, Schema::Integer),
            ("increment", FunctionKind::Mutation, Visibility::Internal, Schema::Null, Schema::Integer),
            ("foreground", FunctionKind::Mutation, Visibility::Public, Schema::Null, Schema::Integer),
            ("schedule", FunctionKind::Mutation, Visibility::Public, schedule.clone(), Schema::String),
            ("rollback", FunctionKind::Mutation, Visibility::Public, schedule.clone(), Schema::String),
            ("badTarget", FunctionKind::Mutation, Visibility::Public, schedule.clone(), Schema::String),
            ("badArgs", FunctionKind::Mutation, Visibility::Public, schedule.clone(), Schema::String),
            ("overflow", FunctionKind::Mutation, Visibility::Public, schedule.clone(), Schema::String),
            ("querySchedule", FunctionKind::Query, Visibility::Public, schedule, Schema::String),
            ("flow", FunctionKind::Action, Visibility::Internal, Schema::Integer, Schema::String),
            ("cancel", FunctionKind::Mutation, Visibility::Public, Schema::String, Schema::Null),
            ("retry", FunctionKind::Mutation, Visibility::Public, retry, Schema::Null),
        ]
        .into_iter()
        .map(|(name, kind, visibility, arguments, result)| {
            (name.into(), Function { kind, visibility, export: name.into(), arguments, result })
        })
        .collect(),
    }
}
async fn job(backend: &Backend, id: &str) -> Job {
    backend.job(id.into(), json!({"player":"alice"}).into()).await.unwrap()
}
pub(super) async fn state(backend: &Backend, id: &str, expected: JobState) -> Job {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let job = job(backend, id).await;
            if job.state == expected {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
async fn count(backend: &Backend, player: &str) -> i64 {
    serde_json::from_str(&backend.query(call("old", "read", player, json!(null))).await.unwrap().json).unwrap()
}
pub(super) async fn schedule(backend: &Backend, operation: &str, at: i64, delay: i64) -> String {
    serde_json::from_str(
        &backend
            .mutate(operation.into(), call("old", "schedule", "alice", json!({"at":at,"delay":delay})))
            .await
            .unwrap()
            .json,
    )
    .unwrap()
}

#[tokio::test]
async fn jobs_commit_with_mutations_and_timer_dispatch_preserves_origin_and_foreground_progress() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    let args = json!({"at":now()+3000,"delay":500});
    for function in ["rollback", "badTarget", "badArgs", "overflow"] {
        assert!(backend.mutate(function.into(), call("old", function, "alice", args.clone())).await.is_err());
        assert_eq!(count(&backend, "alice").await, 0);
        assert_eq!(backend.wake_handoff().await.unwrap().generation, 0);
    }
    assert!(backend.query(call("old", "querySchedule", "alice", args)).await.is_err());
    assert!(
        backend
            .mutate("too-far".into(), call("old", "schedule", "alice", json!({"at":i64::MAX,"delay":0})))
            .await
            .is_err()
    );
    let at = now() + 500;
    let id = schedule(&backend, "one", at, 500).await;
    assert_eq!(schedule(&backend, "one", at, 500).await, id);
    assert_eq!(count(&backend, "alice").await, 40);
    assert_eq!(job(&backend, &id).await.state, JobState::Pending);
    assert!(backend.job(id.clone(), json!({"player":"mallory"}).into()).await.is_err());
    assert!(backend.mutate("stolen-cancel".into(), call("old", "cancel", "mallory", json!(id))).await.is_err());
    assert!(matches!(backend.release(DeploymentId::new("old").unwrap()).await, Err(Error::Busy)));
    let wake = backend.wake_handoff().await.unwrap();
    assert_eq!(wake.due_at, Some(at));
    assert!(backend.acknowledge_wake(wake.generation, wake.due_at).await.unwrap().acknowledged);
    backend.deploy(deployment("new", 10)).await.unwrap();
    let mut updates = backend.subscribe(call("old", "read", "alice", json!(null))).await.unwrap();
    updates.next().await.unwrap();
    let update = tokio::time::timeout(Duration::from_secs(3), updates.next()).await.unwrap().unwrap();
    assert_eq!(&*update.json, "41");
    let foreground = tokio::time::timeout(
        Duration::from_millis(250),
        backend.mutate("foreground".into(), call("new", "foreground", "bob", json!(null))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&*foreground.json, "10");
    let completed = state(&backend, &id, JobState::Succeeded).await;
    assert_eq!(completed.result, Some(json!(completed.invocation_id())));
    assert_eq!(count(&backend, "alice").await, 42);
    assert!(backend.acknowledge_wake(wake.generation, wake.due_at).await.is_err());
    drop(updates);
    assert!(backend.release(DeploymentId::new("old").unwrap()).await.unwrap());
    backend.forget_job(id, json!({"player":"alice"}).into()).await.unwrap();
}

#[tokio::test]
async fn restart_recovers_pending_and_marks_running_unknown_without_repeating_effects() {
    let directory = tempfile::tempdir().unwrap();
    let first = backend(&directory);
    first.deploy(deployment("old", 1)).await.unwrap();
    let id = schedule(&first, "recover", now() + 500, 30_000).await;
    drop(first);
    let second = backend(&directory);
    assert_eq!(job(&second, &id).await.attempt, 1);
    tokio::time::timeout(Duration::from_secs(3), async {
        while count(&second, "alice").await != 41 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    drop(second);
    let third = backend(&directory);
    let recovered = state(&third, &id, JobState::Unknown).await;
    assert_eq!(recovered.attempt, 1);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(count(&third, "alice").await, 41);
    assert!(
        third
            .mutate("no-ack".into(), call("old", "retry", "alice", json!({"id":id,"at":now(),"ack":false})))
            .await
            .is_err()
    );
    third
        .mutate("retry".into(), call("old", "retry", "alice", json!({"id":id,"at":now()+50,"ack":true})))
        .await
        .unwrap();
    state(&third, &id, JobState::Running).await;
    tokio::time::timeout(Duration::from_secs(3), async {
        while count(&third, "alice").await != 42 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    third.mutate("cancel-running".into(), call("old", "cancel", "alice", json!(id))).await.unwrap();
    let cancelled = state(&third, &id, JobState::Unknown).await;
    assert_eq!(cancelled.attempt, 2);
    assert_ne!(cancelled.invocation_id(), recovered.invocation_id());
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(count(&third, "alice").await, 42);
    assert!(third.release(DeploymentId::new("old").unwrap()).await.unwrap());
    drop(third);
    let store = SqliteStore::open(directory.path().join("jobs.db"), "jobs").unwrap();
    let fingerprint =
        Sha256::digest(serde_json::to_vec(&("mutation-v2", "increment", "null", r#"{"player":"alice"}"#)).unwrap())
            .into();
    for attempt in 1..=2 {
        let operation = chunk_store::Operation { id: format!("job/{id}/attempt/{attempt}/1"), fingerprint };
        assert!(store.outcome(&operation).unwrap().is_some());
    }
}

#[tokio::test]
async fn pending_cancellation_prevents_dispatch_and_releases_retained_code() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    let id = schedule(&backend, "pending", now() + 1000, 0).await;
    assert!(backend.forget_job(id.clone(), json!({"player":"alice"}).into()).await.is_err());
    backend.mutate("cancel".into(), call("old", "cancel", "alice", json!(id))).await.unwrap();
    assert_eq!(job(&backend, &id).await.state, JobState::Cancelled);
    assert_eq!(backend.wake_handoff().await.unwrap().due_at, None);
    assert!(backend.release(DeploymentId::new("old").unwrap()).await.unwrap());
    drop(backend);
    let restarted = self::backend(&directory);
    assert_eq!(job(&restarted, &id).await.state, JobState::Cancelled);
    restarted.forget_job(id.clone(), json!({"player":"alice"}).into()).await.unwrap();
    assert!(restarted.job(id, json!({"player":"alice"}).into()).await.is_err());
}

#[tokio::test]
async fn exhausted_action_capacity_leaves_due_jobs_pending_until_a_worker_is_available() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    let mut version = deployment("old", 1);
    version.source.push_str("\nexport async function wait(ctx) { await ctx.sleep(30000); return null; }");
    version.functions.insert(
        "wait".into(),
        Function {
            kind: FunctionKind::Action,
            visibility: Visibility::Public,
            export: "wait".into(),
            arguments: Schema::Null,
            result: Schema::Null,
        },
    );
    backend.deploy(version).await.unwrap();
    let mut actions = Vec::new();
    for _ in 0..8 {
        actions.push(
            backend
                .start_action(backend.allocate_action_id().await.unwrap(), call("old", "wait", "blocker", json!(null)))
                .await
                .unwrap(),
        );
    }
    let id = schedule(&backend, "capacity", now(), 0).await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(job(&backend, &id).await.state, JobState::Pending);
    let mut released = actions.pop().unwrap();
    released.cancel();
    assert!(released.outcome().await.is_err());
    let completed = state(&backend, &id, JobState::Succeeded).await;
    assert_eq!(completed.attempt, 1);
    assert_eq!(count(&backend, "alice").await, 42);
    for mut action in actions {
        action.cancel();
        assert!(action.outcome().await.is_err());
    }
}

#[tokio::test]
async fn observed_application_failure_is_terminal_and_does_not_undo_prior_effects() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    let id = schedule(&backend, "failed", now(), -1).await;
    let failed = state(&backend, &id, JobState::Failed).await;
    assert!(failed.result.is_none());
    assert_eq!(count(&backend, "alice").await, 41);
}

#[tokio::test]
async fn retiring_a_deployment_frees_its_slot_despite_a_scheduled_job_and_a_subscription() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    for index in 1..crate::MAX_DEPLOYMENTS {
        backend.deploy(deployment(&format!("d{index}"), 1)).await.unwrap();
    }
    let id = schedule(&backend, "one", now() + 60_000, 0).await;
    let mut group = backend.subscribe_group(vec![call("old", "read", "alice", json!(null))]).await.unwrap();
    group.next().await.unwrap();
    assert!(matches!(backend.deploy(deployment("next", 1)).await, Err(Error::Busy)));
    assert!(matches!(backend.release(DeploymentId::new("old").unwrap()).await, Err(Error::Busy)));

    assert!(backend.retire(DeploymentId::new("old").unwrap()).await.unwrap());
    assert!(matches!(group.next().await, Err(Error::Retired)));
    assert_eq!(job(&backend, &id).await.state, JobState::Cancelled);
    assert!(backend.query(call("old", "read", "alice", json!(null))).await.is_err());
    backend.deploy(deployment("next", 1)).await.unwrap();
}

#[tokio::test]
async fn a_retirement_completes_though_its_caller_stops_waiting() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    let old = DeploymentId::new("old").unwrap();
    // The caller stops waiting at once, though the retirement may already have finished.
    _ = tokio::time::timeout(Duration::ZERO, backend.retire(old)).await;
    for _ in 0..100 {
        if backend.deployments().await.unwrap().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the deployment was never released");
}

#[tokio::test]
async fn a_restart_after_the_retirement_committed_completes_it_and_cancels_a_late_job() {
    let directory = tempfile::tempdir().unwrap();
    let first = backend(&directory);
    first.deploy(deployment("old", 1)).await.unwrap();
    drop(first);
    let mut store = SqliteStore::open(directory.path().join("jobs.db"), "jobs").unwrap();
    store.job_command(chunk_store::JobCommand::CancelDeployment { deployment: "old".into() }).unwrap();
    // Work admitted before the retirement schedules a job after it committed.
    let late = Job {
        id: "late".into(),
        deployment: "old".into(),
        function: "flow".into(),
        arguments: json!(0),
        caller: json!({"player":"alice"}),
        due_at: now(),
        attempt: 1,
        state: JobState::Pending,
        result: None,
    };
    let operation = chunk_store::Operation { id: "late".into(), fingerprint: [0; 32] };
    let expected = store.snapshot().unwrap().revision;
    let commit = chunk_store::Commit { expected, operation, writes: vec![], result: json!(null) };
    store.commit_with_jobs(commit, vec![chunk_store::JobIntent::Schedule(late)]).unwrap();
    drop(store);
    let restarted = backend(&directory);
    for _ in 0..100 {
        if restarted.deployments().await.unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(restarted.deployments().await.unwrap().is_empty());
    assert_eq!(job(&restarted, "late").await.state, JobState::Cancelled);
}
