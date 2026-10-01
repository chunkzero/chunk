use std::sync::mpsc;

use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::DeploymentId;
use chunk_store::{IndexDefinition, SqliteStore, Work};
use serde_json::{Value, json};
use tokio::sync::mpsc as signals;

use super::ControlledStore;
use crate::{Backend, Call, Error, Readiness};

const SOURCE: &str = r"
export function ranked(ctx) {
  return ctx.db.scanIndex({table: 'scores', index: 'ranked', limit: 10}).map(([id]) => id).join(',');
}
export function seed(ctx) { ctx.db.put('scores', 'x', {a: 1, b: 2}); ctx.db.put('scores', 'y', {a: 2, b: 1}); return ''; }
";

/// A deployment whose `ranked` index orders scores by `field`.
fn deployment(id: &str, field: &str) -> Deployment {
    let function = |kind| Function {
        kind,
        visibility: Visibility::Public,
        export: String::new(),
        arguments: Schema::Null,
        result: Schema::String,
    };
    let functions = [("ranked", FunctionKind::Query), ("seed", FunctionKind::Mutation)];
    Deployment {
        contracts: Contracts::default(),
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: SOURCE.into(),
        tables: serde_json::from_value(json!({"scores": {
            "fields": {"a": {"schema": {"type": "integer"}}, "b": {"schema": {"type": "integer"}}},
            "indexes": {"ranked": [field]},
        }}))
        .unwrap(),
        functions: functions
            .into_iter()
            .map(|(name, kind)| (name.into(), Function { export: name.into(), ..function(kind) }))
            .collect(),
    }
}

fn call(deployment: &str, function: &str) -> Call {
    Call {
        deployment: DeploymentId::new(deployment).unwrap(),
        function: function.into(),
        arguments: Value::Null.into(),
        caller: Value::Null.into(),
    }
}

async fn ranked(backend: &Backend, deployment: &str) -> String {
    serde_json::from_str(&backend.query(call(deployment, "ranked")).await.unwrap().json).unwrap()
}

fn ranked_by(field: &str) -> IndexDefinition {
    IndexDefinition { table: "scores".into(), name: "ranked".into(), fields: vec![field.into()] }
}

#[tokio::test]
async fn pending_work_resumes_after_a_restart_before_the_deployment_serves() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("ready.db");
    let (notices, _notices) = signals::unbounded_channel();
    let (building, gate) = mpsc::channel();
    let store = ControlledStore {
        work: Some(gate),
        ..ControlledStore::new(SqliteStore::open(&path, "local").unwrap(), notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.install(deployment("one", "a")).await.unwrap();
    let id = DeploymentId::new("one").unwrap();
    assert!(matches!(backend.query(call("one", "ranked")).await, Err(Error::NotReady)));
    let readiness = backend.readiness(id.clone()).await.unwrap();
    assert!(!readiness.ready);
    assert_eq!(
        readiness.pending.iter().map(|pending| &pending.work).collect::<Vec<_>>(),
        [&Work::Index(ranked_by("a"))]
    );
    // Core stops while the index builds.
    drop(building);
    drop(backend);

    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    backend.ready(id.clone()).await.unwrap();
    assert_eq!(backend.readiness(id).await.unwrap(), Readiness { ready: true, pending: Vec::new() });
    backend.mutate("seed".into(), call("one", "seed")).await.unwrap();
    assert_eq!(ranked(&backend, "one").await, "x,y");
}

#[tokio::test]
async fn deployments_declaring_one_index_name_with_different_fields_each_read_their_own() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("indexes.db");
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    backend.deploy(deployment("one", "a")).await.unwrap();
    backend.mutate("seed".into(), call("one", "seed")).await.unwrap();
    backend.deploy(deployment("two", "b")).await.unwrap();
    assert_eq!(ranked(&backend, "one").await, "x,y");
    assert_eq!(ranked(&backend, "two").await, "y,x");
    let physical = || -> Vec<String> {
        let connection = rusqlite::Connection::open(&path).unwrap();
        let mut statement = connection
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'index' AND tbl_name = 'scores' AND sql IS NOT NULL ORDER BY name")
            .unwrap();
        statement.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    };
    assert_eq!(physical().len(), 2);
    assert!(backend.release(DeploymentId::new("one").unwrap()).await.unwrap());
    assert_eq!(physical(), ["_chunk_index_73636f726573_72616e6b6564_62"]);
    assert_eq!(ranked(&backend, "two").await, "y,x");
}
