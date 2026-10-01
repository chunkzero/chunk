use std::{sync::mpsc, time::Duration};

use chunk_contract::{
    Contracts, DatabaseSchema, Deployment, Function, FunctionKind, Migration, MigrationKind, MigrationTable,
    RuntimeProfile, Schema, Visibility,
};
use chunk_js::DeploymentId;
use chunk_store::SqliteStore;
use serde_json::{Value, json};
use tokio::sync::mpsc as signals;

use super::ControlledStore;
use crate::{Backend, Call, Error};

/// Both shapes share one bundle; the rename's transforms change the case of the name.
const SOURCE: &str = r"
export function __chunk_migrate(_, {direction, rows}) {
  return rows.map((row) => direction === 'to'
    ? {displayName: row.name.toUpperCase() + (Date.now() === 0 ? '' : '!')}
    : {name: row.displayName.toLowerCase()});
}
export function read(ctx, id) { return JSON.stringify(ctx.db.get('fighters', id)); }
export function writeOld(ctx, id) { ctx.db.put('fighters', id, {name: id + '-old'}); return ''; }
export function writeNew(ctx, id) { ctx.db.put('fighters', id, {displayName: id.toUpperCase() + '-NEW'}); return ''; }
export function seed(ctx, part) {
  for (let i = part * 100; i < part * 100 + 100; i++) ctx.db.put('fighters', String(i).padStart(3, '0'), {name: 'n' + i});
  return '';
}
";

fn schema(field: &str) -> DatabaseSchema {
    serde_json::from_value(json!({"fighters": {"fields": {field: {"schema": {"type": "string"}}}}})).unwrap()
}

/// A deployment whose journal has the first `length` entries of a rename of `fighters.name` to `displayName`.
fn deployment(id: &str, length: usize) -> Deployment {
    let entry = |id: &str, kind, finishes: Option<&str>, schema| {
        let added = if kind == MigrationKind::Expand { vec!["displayName".into()] } else { Vec::new() };
        let change = MigrationTable { added, removed: vec!["name".into()], back: kind == MigrationKind::Expand };
        let tables = if kind == MigrationKind::Baseline {
            std::collections::BTreeMap::new()
        } else {
            [("fighters".into(), change)].into()
        };
        Migration { id: id.into(), hash: "0".repeat(64), kind, finishes: finishes.map(Into::into), tables, schema }
    };
    let mut migrations = vec![
        entry("0001_init", MigrationKind::Baseline, None, schema("name")),
        entry("0002_rename", MigrationKind::Expand, None, schema("displayName")),
        entry("0003_finish_rename", MigrationKind::Finish, Some("0002_rename"), schema("displayName")),
    ];
    migrations.truncate(length);
    let function = |name: &str, kind| {
        let function = Function {
            kind,
            visibility: Visibility::Public,
            export: name.into(),
            arguments: Schema::String,
            result: Schema::String,
        };
        (name.into(), function)
    };
    Deployment {
        contracts: Contracts { migrations: migrations.clone(), ..Contracts::default() },
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: SOURCE.into(),
        tables: migrations.last().unwrap().schema.clone(),
        functions: [
            function("read", FunctionKind::Query),
            function("writeOld", FunctionKind::Mutation),
            function("writeNew", FunctionKind::Mutation),
            function("seed", FunctionKind::Mutation),
        ]
        .into(),
    }
}

fn call(deployment: &str, function: &str, id: &str) -> Call {
    Call {
        deployment: DeploymentId::new(deployment).unwrap(),
        function: function.into(),
        arguments: json!(id).into(),
        caller: Value::Null.into(),
    }
}

async fn read(backend: &Backend, deployment: &str, id: &str) -> Value {
    let json: String = serde_json::from_str(&backend.query(call(deployment, "read", id)).await.unwrap().json).unwrap();
    serde_json::from_str(&json).unwrap()
}

async fn write(backend: &Backend, deployment: &str, function: &str, id: &str) {
    backend.mutate(format!("{deployment}-{function}-{id}"), call(deployment, function, id)).await.unwrap();
}

#[tokio::test]
async fn a_rename_backfills_syncs_both_ways_and_drops_the_old_field_once_finished() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rename.db");
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    backend.deploy(deployment("old", 1)).await.unwrap();
    write(&backend, "old", "writeOld", "a").await;
    backend.deploy(deployment("new", 2)).await.unwrap();
    assert_eq!(read(&backend, "new", "a").await, json!({"displayName": "A-OLD"}));
    assert_eq!(read(&backend, "old", "a").await, json!({"name": "a-old"}));

    write(&backend, "old", "writeOld", "b").await;
    assert_eq!(read(&backend, "new", "b").await, json!({"displayName": "B-OLD"}));
    write(&backend, "new", "writeNew", "a").await;
    assert_eq!(read(&backend, "old", "a").await, json!({"name": "a-new"}));

    backend.deploy(deployment("finished", 3)).await.unwrap();
    assert!(backend.release(DeploymentId::new("old").unwrap()).await.unwrap());
    let columns = || -> Vec<String> {
        let connection = rusqlite::Connection::open(&path).unwrap();
        let mut statement = connection.prepare("SELECT name FROM pragma_table_info('fighters')").unwrap();
        statement.query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap()
    };
    for _ in 0..100 {
        if !columns().contains(&"name".to_owned()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!columns().contains(&"name".to_owned()), "{:?}", columns());
    assert_eq!(read(&backend, "finished", "a").await, json!({"displayName": "A-NEW"}));
    let error = backend.deploy(deployment("rollback", 1)).await.unwrap_err();
    assert!(matches!(&error, Error::Migration(message) if message.contains("fighters.name")), "{error}");
}

#[tokio::test]
async fn a_write_racing_the_backfill_keeps_its_value() {
    let directory = tempfile::tempdir().unwrap();
    let (notices, _notices) = signals::unbounded_channel();
    let (gate, held) = mpsc::channel();
    let store = ControlledStore {
        work: Some(held),
        ..ControlledStore::new(SqliteStore::open(directory.path().join("race.db"), "local").unwrap(), notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.deploy(deployment("old", 1)).await.unwrap();
    for part in ["0", "1", "2"] {
        write(&backend, "old", "seed", part).await;
    }
    // The backfill's first batch waits on the gate, so this write commits while the backfill is underway.
    backend.install(deployment("new", 2)).await.unwrap();
    let racing = tokio::spawn({
        let backend = backend.clone();
        async move { write(&backend, "old", "writeOld", "299").await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.send(()).unwrap();
    racing.await.unwrap();
    backend.ready(DeploymentId::new("new").unwrap()).await.unwrap();
    assert_eq!(read(&backend, "new", "299").await, json!({"displayName": "299-OLD"}));
    assert_eq!(read(&backend, "new", "000").await, json!({"displayName": "N0"}));
}
