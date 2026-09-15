use std::{fmt::Write, time::Duration};

use chunk_contract::{Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::DeploymentId;
use chunk_store::SqliteStore;
use serde_json::json;

use crate::{ActionStatus, Backend, Call, Error};

fn deployment(id: &str, increment: i32) -> Deployment {
    let mut deployment = Deployment {
        session_methods: None,
        domains: None,
        destinations: None,
        contract_version: 2,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: format!(
            r"
export function read(ctx) {{ return ctx.db.get('counts',ctx.caller.player)?.value ?? 0; }}
export function increment(ctx) {{ const value=read(ctx)+{increment}; ctx.db.put('counts',ctx.caller.player,{{value}}); return value; }}
export function canFinish(ctx) {{ return (ctx.db.get('counts','release-'+ctx.caller.player)?.value ?? 0) > 0; }}
export async function flow(ctx, delay) {{
  if ('db' in ctx || typeof fetch !== 'undefined' || typeof setTimeout !== 'undefined') throw Error('ambient capability');
  await ctx.runMutation('increment', null);
  if (delay < 0) {{
    while (!await ctx.runQuery('canFinish', null)) await ctx.sleep(100);
  }} else await ctx.sleep(delay);
  await ctx.runMutation('increment', null);
  return ctx.runQuery('privateRead', null);
}}
export async function tooMany(ctx) {{ await Promise.all(Array.from({{length:9}},()=>ctx.sleep(1000))); return 0; }}
export async function spin(ctx) {{ await ctx.runMutation('increment', null); while(true) {{}} }}
"
        ),
        tables: serde_json::from_value(json!({"counts":{"fields":{"value":{"schema":{"type":"integer"}}}}})).unwrap(),
        functions: [
            ("read", "read", FunctionKind::Query, Visibility::Public),
            ("privateRead", "read", FunctionKind::Query, Visibility::Internal),
            ("increment", "increment", FunctionKind::Mutation, Visibility::Internal),
            ("publicIncrement", "increment", FunctionKind::Mutation, Visibility::Public),
            ("flow", "flow", FunctionKind::Action, Visibility::Public),
            ("tooMany", "tooMany", FunctionKind::Action, Visibility::Public),
            ("spin", "spin", FunctionKind::Action, Visibility::Public),
            ("canFinish", "canFinish", FunctionKind::Query, Visibility::Internal),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, (name, _export, kind, visibility))| {
            // Deployment manifests require a unique export per descriptor.
            (
                name.into(),
                Function {
                    kind,
                    visibility,
                    export: format!("f{index}"),
                    arguments: if name == "flow" { Schema::Integer } else { Schema::Null },
                    result: if name == "canFinish" { Schema::Boolean } else { Schema::Integer },
                },
            )
        })
        .collect(),
    };
    for (index, export) in
        ["read", "read", "increment", "increment", "flow", "tooMany", "spin", "canFinish"].iter().enumerate()
    {
        write!(deployment.source, "\nexport const f{index} = {export};").unwrap();
    }
    deployment
}

fn call(version: &str, function: &str, player: &str, arguments: serde_json::Value) -> Call {
    Call {
        deployment: DeploymentId::new(version).unwrap(),
        function: function.into(),
        arguments: arguments.into(),
        caller: json!({"player":player}).into(),
    }
}

fn backend(directory: &tempfile::TempDir) -> Backend {
    Backend::new("test".into(), Box::new(SqliteStore::open(directory.path().join("actions.db"), "test").unwrap()))
        .unwrap()
}

#[tokio::test]
async fn sleeping_actions_yield_to_transactions_and_retain_caller_deployment_and_identity() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    let mut alice = backend.subscribe(call("old", "read", "alice", json!(null))).await.unwrap();
    alice.next().await.unwrap();
    let id = backend.allocate_action_id().unwrap();
    let request = call("old", "flow", "alice", json!(-1));
    let mut action = backend.start_action(id.clone(), request.clone()).await.unwrap();
    let update = tokio::time::timeout(Duration::from_secs(5), alice.next()).await.unwrap().unwrap();
    assert_eq!(&*update.json, "1");
    assert!(matches!(backend.release(DeploymentId::new("old").unwrap()).await, Err(Error::Busy)));
    backend.deploy(deployment("new", 10)).await.unwrap();
    let foreground = tokio::time::timeout(
        Duration::from_secs(5),
        backend.mutate("foreground".into(), call("new", "publicIncrement", "bob", json!(null))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&*foreground.json, "10");
    assert!(matches!(action.status(), ActionStatus::Running));
    let mut duplicate = backend.start_action(id.clone(), request).await.unwrap();
    assert!(matches!(
        backend.start_action(id.clone(), call("old", "flow", "mallory", json!(-1))).await,
        Err(Error::OperationMismatch)
    ));
    assert!(matches!(
        backend.action_status(id.clone(), json!({"player":"mallory"}).into()).await,
        Err(Error::ActionOutcomeUnknown)
    ));
    backend.mutate("release-flow".into(), call("old", "publicIncrement", "release-alice", json!(null))).await.unwrap();
    assert_eq!(&*action.outcome().await.unwrap(), "2");
    assert_eq!(&*duplicate.outcome().await.unwrap(), "2");
    assert_eq!(&*backend.query(call("new", "read", "alice", json!(null))).await.unwrap().json, "2");
    assert!(matches!(backend.query(call("old", "privateRead", "alice", json!(null))).await, Err(Error::Unknown)));
    assert!(matches!(
        backend.mutate("spoof".into(), call("old", "increment", "alice", json!(null))).await,
        Err(Error::Unknown)
    ));
    assert!(matches!(backend.query(call("old", "flow", "alice", json!(500))).await, Err(Error::Contract)));
    drop(alice);
    assert!(backend.release(DeploymentId::new("old").unwrap()).await.unwrap());
}

#[tokio::test]
async fn cancellation_expires_sleep_and_cpu_scopes_without_undoing_committed_work() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.deploy(deployment("old", 1)).await.unwrap();
    for function in ["flow", "spin"] {
        let mut updates = backend.subscribe(call("old", "read", function, json!(null))).await.unwrap();
        updates.next().await.unwrap();
        let arguments = if function == "flow" { json!(30_000) } else { json!(null) };
        let mut action = backend
            .start_action(backend.allocate_action_id().unwrap(), call("old", function, function, arguments))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), updates.next()).await.unwrap().unwrap();
        action.cancel();
        assert!(tokio::time::timeout(Duration::from_secs(2), action.outcome()).await.unwrap().is_err());
        assert_eq!(&*backend.query(call("old", "read", function, json!(null))).await.unwrap().json, "1");
    }
    let mut exhausted = backend
        .start_action(backend.allocate_action_id().unwrap(), call("old", "tooMany", "alice", json!(null)))
        .await
        .unwrap();
    assert!(exhausted.outcome().await.unwrap_err().to_string().contains("capacity"));
}

#[tokio::test]
async fn backend_loss_keeps_partial_mutations_but_action_identity_cannot_restart() {
    let directory = tempfile::tempdir().unwrap();
    let first = backend(&directory);
    first.deploy(deployment("old", 1)).await.unwrap();
    let mut updates = first.subscribe(call("old", "read", "alice", json!(null))).await.unwrap();
    updates.next().await.unwrap();
    let id = first.allocate_action_id().unwrap();
    let request = call("old", "flow", "alice", json!(30_000));
    let mut action = first.start_action(id.clone(), request.clone()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), updates.next()).await.unwrap().unwrap();
    drop(updates);
    drop(first);
    assert!(matches!(action.outcome().await, Err(Error::ActionOutcomeUnknown)));
    let restarted = backend(&directory);
    assert_eq!(&*restarted.query(call("old", "read", "alice", json!(null))).await.unwrap().json, "1");
    assert!(matches!(restarted.start_action(id.clone(), request).await, Err(Error::ActionOutcomeUnknown)));
    assert!(matches!(
        restarted.action_status(id, json!({"player":"alice"}).into()).await,
        Err(Error::ActionOutcomeUnknown)
    ));
}

#[tokio::test]
async fn action_capacity_and_status_retirement_never_restart_evicted_invocations() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    let mut version = deployment("old", 1);
    version.source.push_str("\nexport function complete() { return 42; }");
    version.functions.insert(
        "complete".into(),
        Function {
            kind: FunctionKind::Action,
            visibility: Visibility::Public,
            export: "complete".into(),
            arguments: Schema::Null,
            result: Schema::Integer,
        },
    );
    backend.deploy(version).await.unwrap();
    let mut actions = Vec::new();
    for player in 0..8 {
        actions.push(
            backend
                .start_action(
                    backend.allocate_action_id().unwrap(),
                    call("old", "flow", &player.to_string(), json!(30_000)),
                )
                .await
                .unwrap(),
        );
    }
    assert!(matches!(
        backend
            .start_action(backend.allocate_action_id().unwrap(), call("old", "flow", "overflow", json!(30_000)))
            .await,
        Err(Error::Busy)
    ));
    for action in &mut actions {
        action.cancel();
        assert!(action.outcome().await.is_err());
    }
    let first = backend.allocate_action_id().unwrap();
    for index in 0..33 {
        let id = if index == 0 { first.clone() } else { backend.allocate_action_id().unwrap() };
        let mut action = backend.start_action(id, call("old", "complete", "alice", json!(null))).await.unwrap();
        assert_eq!(&*action.outcome().await.unwrap(), "42");
    }
    assert!(matches!(
        backend.start_action(first.clone(), call("old", "complete", "alice", json!(null))).await,
        Err(Error::ActionOutcomeUnknown)
    ));
    assert!(matches!(
        backend.action_status(first, json!({"player":"alice"}).into()).await,
        Err(Error::ActionOutcomeUnknown)
    ));
}
