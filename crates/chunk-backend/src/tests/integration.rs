use std::time::Duration;

use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::DeploymentId;
use chunk_store::{SqliteStore, Storage};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::{Backend, Call, Error};

const SOURCE: &str = r"
export function get(ctx) { return ctx.db.get('counters', 'count')?.value ?? 0; }
export function strict(ctx) { return ctx.db.get('counters', 'count').value; }
export function increment(ctx) { const value=get(ctx)+1; ctx.db.put('counters','count',{value}); return value; }
export function badResult(ctx) { ctx.db.put('counters','count',{value:999}); return 'invalid'; }
export function privateRead() { return 123; }
";

pub(super) fn deployment(id: &str) -> Deployment {
    Deployment {
        contracts: Contracts::default(),
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: SOURCE.into(),
        tables: serde_json::from_value(json!({"counters": {"fields": {"value": {"schema": {"type": "integer"}}}}}))
            .unwrap(),
        functions: [
            ("get", FunctionKind::Query),
            ("strict", FunctionKind::Query),
            ("increment", FunctionKind::Mutation),
            ("badResult", FunctionKind::Mutation),
            ("privateRead", FunctionKind::Query),
        ]
        .into_iter()
        .map(|(name, kind)| {
            (
                name.into(),
                Function {
                    kind,
                    visibility: if name == "privateRead" { Visibility::Internal } else { Visibility::Public },
                    export: name.into(),
                    arguments: Schema::Null,
                    result: Schema::Integer,
                },
            )
        })
        .collect(),
    }
}

/// A call to `function` of deployment `a` with `arguments`.
fn call(function: &str, arguments: Value) -> Call {
    Call {
        deployment: DeploymentId::new("a").unwrap(),
        function: function.into(),
        arguments: arguments.into(),
        caller: json!({"service": "test"}).into(),
    }
}

fn open(path: &std::path::Path) -> Backend {
    Backend::new("local".into(), Box::new(SqliteStore::open(path, "local").unwrap())).unwrap()
}

fn mismatched(error: &Error) -> bool {
    matches!(error, Error::OperationMismatch)
        || matches!(error, Error::Storage(error) if matches!(error.as_ref(), chunk_store::Error::OperationMismatch))
}

#[tokio::test]
async fn a_lost_mutation_reply_is_recovered_by_its_operation_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backend.db");
    let mut store = SqliteStore::open(&path, "local").unwrap();
    store.apply_schema(&deployment("a").tables).unwrap();
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.deploy(deployment("a")).await.unwrap();
    backend.deploy(deployment("b")).await.unwrap();
    let queries = || vec![call("get", Value::Null), call("strict", Value::Null)];
    let mut group = backend.subscribe_group(queries()).await.unwrap();
    let initial = group.next().await.unwrap();
    assert_eq!(initial.results[0].as_ref().unwrap(), "0");
    assert!(initial.results[1].is_err());
    // Discard the successful reply: the caller recovers it by operation after restart.
    let revision = backend.mutate("lost-reply".into(), call("increment", Value::Null)).await.unwrap().revision;
    let update = group.next().await.unwrap();
    assert_eq!(update.revision, revision);
    let results: Vec<_> = update.results.into_iter().map(|result| result.unwrap()).collect();
    assert_eq!(results, ["1", "1"]);
    drop(group);
    drop(backend);

    let backend = open(&path);
    // Retained bundles and contracts reload without registration or application input.
    let mut changed = deployment("a");
    changed.source.push_str("\n// changed");
    assert!(backend.deploy(changed).await.is_err());
    let recovered = backend.mutate("lost-reply".into(), call("increment", Value::Null)).await.unwrap();
    assert_eq!((recovered.revision, &*recovered.json), (revision, "1"));
    let mut mismatch = call("increment", Value::Null);
    mismatch.caller = json!({}).into();
    assert!(mismatched(&backend.mutate("lost-reply".into(), mismatch).await.unwrap_err()));
    let fresh = backend.subscribe_group(queries()).await.unwrap().next().await.unwrap();
    assert_eq!(fresh.revision, revision);
    let results: Vec<_> = fresh.results.into_iter().map(|result| result.unwrap()).collect();
    assert_eq!(results, ["1", "1"]);
}

#[tokio::test]
async fn optional_null_arguments_recover_the_same_mutation_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("normalized.db");
    let backend = open(&path);
    let mut version = deployment("a");
    version.functions.get_mut("increment").unwrap().arguments = Schema::Object {
        fields: [("note".into(), chunk_contract::Field { schema: Schema::String, optional: true })].into(),
    };
    version.source.push_str("\nexport function optional(ctx, args) { if (Object.hasOwn(args, 'note')) throw Error('expected omission'); return increment(ctx); }");
    version.functions.get_mut("increment").unwrap().export = "optional".into();
    backend.deploy(version).await.unwrap();
    let first = backend.mutate("normalized-retry".into(), call("increment", json!({"note": null}))).await.unwrap();
    drop(backend);

    let backend = open(&path);
    let retry = backend.mutate("normalized-retry".into(), call("increment", json!({}))).await.unwrap();
    assert_eq!((retry.revision, &*retry.json), (first.revision, &*first.json));
    let changed = backend.mutate("normalized-retry".into(), call("increment", json!({"note": "changed"}))).await;
    assert!(mismatched(&changed.unwrap_err()));
    assert_eq!(&*backend.query(call("get", Value::Null)).await.unwrap().json, "1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_closes_subscriptions_joins_the_backend_and_releases_its_database() {
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("bundle.json");
    std::fs::write(&bundle, serde_json::to_vec(&deployment("a")).unwrap()).unwrap();
    // Reopening the same database proves the backend joined its threads.
    for _ in 0..2 {
        let stop = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let config = crate::server::Config {
            bundle: Some(bundle.clone()),
            environment: "local".into(),
            vars: None,
            secrets: crate::Secrets::default(),
            state: directory.path().join("state"),
            replication: None,
            fork: None,
        };
        let task = tokio::spawn(crate::server::run(config, ready, stop.clone()));
        // The embedder keeps its readiness handle past shutdown.
        let crate::server::Ready { backend, deployment, .. } =
            tokio::time::timeout(Duration::from_secs(10), started).await.unwrap().unwrap();
        assert_eq!(deployment, "a");
        let mut group = backend.subscribe_group(vec![call("get", Value::Null)]).await.unwrap();
        assert!(group.next().await.is_ok());
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(10), group.next()).await.unwrap().is_err());
        assert!(matches!(backend.wake_handoff().await, Err(Error::Closed)));
    }
}
