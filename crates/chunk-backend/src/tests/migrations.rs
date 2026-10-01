use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

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
  return rows.map((row) => {
    const shape = direction === 'to' ? ['_id', 'name'] : ['_id', 'displayName'];
    if (Object.keys(row).some((key) => !shape.includes(key))) throw new Error('unprojected ' + JSON.stringify(row));
    if (row.name?.startsWith('bad')) throw new Error('bad row');
    return direction === 'to'
      ? {displayName: row.name.toUpperCase() + (Date.now() === 0 ? '' : '!')}
      : {name: row.displayName.toLowerCase()};
  });
}
export function read(ctx, id) { return JSON.stringify(ctx.db.get('fighters', id)); }
export function writeOld(ctx, id) { ctx.db.put('fighters', id, {name: id + '-old'}); return ''; }
export function writeBig(ctx, id) { ctx.db.put('fighters', id, {name: 'a'.repeat(1048496)}); return ''; }
export function writeLarge(ctx, id) { ctx.db.put('fighters', id, {name: 'a'.repeat(400000)}); return ''; }
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
            function("writeBig", FunctionKind::Mutation),
            function("writeLarge", FunctionKind::Mutation),
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
    assert!(matches!(&error, Error::Migration(message) if message.contains("0002_rename")), "{error}");
}

#[tokio::test]
async fn a_failed_backfill_holds_back_the_drop_and_retries_without_panicking() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("failed.db");
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    backend.deploy(deployment("old", 1)).await.unwrap();
    write(&backend, "old", "writeOld", "bad").await;
    let error = backend.deploy(deployment("new", 2)).await.unwrap_err();
    assert!(matches!(&error, Error::Migration(message) if message.contains("bad row")), "{error}");
    backend.install(deployment("finished", 3)).await.unwrap();
    assert!(backend.release(DeploymentId::new("old").unwrap()).await.unwrap());
    let error = backend.ready(DeploymentId::new("finished").unwrap()).await.unwrap_err();
    assert!(matches!(&error, Error::Migration(message) if message.contains("bad row")), "{error}");
    let connection = rusqlite::Connection::open(&path).unwrap();
    let mut statement = connection.prepare("SELECT name FROM pragma_table_info('fighters')").unwrap();
    let columns: Vec<String> = statement.query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
    assert!(columns.contains(&"name".to_owned()), "{columns:?}");
}

#[tokio::test]
async fn a_write_racing_the_backfill_commits_before_the_backfill_reaches_its_row() {
    let directory = tempfile::tempdir().unwrap();
    let (notices, _notices) = signals::unbounded_channel();
    let open = Arc::new(AtomicBool::new(false));
    let store = ControlledStore {
        pause: Some((open.clone(), 0)),
        ..ControlledStore::new(SqliteStore::open(directory.path().join("race.db"), "local").unwrap(), notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.deploy(deployment("old", 1)).await.unwrap();
    for part in ["0", "1", "2"] {
        write(&backend, "old", "seed", part).await;
    }
    let new = DeploymentId::new("new").unwrap();
    backend.install(deployment("new", 2)).await.unwrap();
    let done = || async { backend.readiness(new.clone()).await.unwrap().pending[0].done };
    while done().await < 256 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    write(&backend, "old", "writeOld", "299").await;
    assert_eq!(done().await, 256, "the write committed while the backfill had not reached its row");
    open.store(true, Ordering::SeqCst);
    backend.ready(new).await.unwrap();
    assert_eq!(read(&backend, "new", "299").await, json!({"displayName": "299-OLD"}));
    assert_eq!(read(&backend, "new", "000").await, json!({"displayName": "N0"}));
}

/// The rename's deployment of `length` entries, whose `to` returns the fields `body` lists.
fn transforming(id: &str, length: usize, body: &str) -> Deployment {
    let migrate = format!("export function __chunk_migrate(_, {{rows}}) {{ return rows.map((row) => ({{{body}}})); }}");
    let source = SOURCE.replacen("export function __chunk_migrate", "function unused", 1);
    Deployment { source: format!("{source}\n{migrate}"), ..deployment(id, length) }
}

#[tokio::test]
async fn a_transform_gives_the_same_result_in_a_backfill_and_in_a_single_row_sync() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("local".into(), Box::new(SqliteStore::open(directory.path().join("seed.db"), "local").unwrap()))
            .unwrap();
    backend.deploy(transforming("old", 1, "displayName: crypto.randomUUID()")).await.unwrap();
    for id in ["a", "b"] {
        write(&backend, "old", "writeOld", id).await;
    }
    backend.deploy(transforming("new", 2, "displayName: crypto.randomUUID()")).await.unwrap();
    write(&backend, "old", "writeOld", "c").await;
    let [a, b, c] = ["a", "b", "c"].map(|id| read(&backend, "new", id));
    let (a, b, c) = (a.await, b.await, c.await);
    assert_eq!((&a, &b), (&b, &c), "the batch position and the path don't change the result");
}

#[tokio::test]
async fn a_maximum_size_row_fits_the_migration_invocation_limits() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("local".into(), Box::new(SqliteStore::open(directory.path().join("big.db"), "local").unwrap()))
            .unwrap();
    backend.deploy(transforming("old", 1, "displayName: 'x'")).await.unwrap();
    write(&backend, "old", "writeBig", "big").await;
    backend.deploy(transforming("new", 2, "displayName: 'x'")).await.unwrap();
    assert_eq!(read(&backend, "new", "big").await, json!({"displayName": "x"}));
}

/// The journals of three deployments: `name`; `name` and an optional `nickname`; and `name` with a required
/// `displayName` that `to` takes from the nickname or the name.
fn nicknames(id: &str, length: usize) -> Deployment {
    let fields = |names: &[(&str, bool)]| -> DatabaseSchema {
        let fields: serde_json::Map<_, _> = names
            .iter()
            .map(|(name, optional)| ((*name).to_owned(), json!({"schema": {"type": "string"}, "optional": optional})))
            .collect();
        serde_json::from_value(json!({"fighters": {"fields": fields}})).unwrap()
    };
    let entry = |id: &str, kind, tables, schema| Migration {
        id: id.into(),
        hash: "0".repeat(64),
        kind,
        finishes: None,
        tables,
        schema,
    };
    let change = MigrationTable { added: vec!["displayName".into()], removed: vec!["nickname".into()], back: true };
    let mut migrations = vec![
        entry("0001_init", MigrationKind::Baseline, std::collections::BTreeMap::new(), fields(&[("name", false)])),
        entry(
            "0002_nickname",
            MigrationKind::Additive,
            std::collections::BTreeMap::new(),
            fields(&[("name", false), ("nickname", true)]),
        ),
        entry(
            "0003_display_name",
            MigrationKind::Expand,
            [("fighters".into(), change)].into(),
            fields(&[("name", false), ("displayName", false)]),
        ),
    ];
    migrations.truncate(length);
    let source = r"
export function __chunk_migrate(_, {direction, rows}) {
  return rows.map((row) => direction === 'to' ? {displayName: row.nickname ?? row.name} : {nickname: row.displayName});
}
export function read(ctx, id) { return JSON.stringify(ctx.db.get('fighters', id)); }
export function writeName(ctx, id) { ctx.db.put('fighters', id, {name: id}); return ''; }
export function writeNickname(ctx, id) { ctx.db.put('fighters', id, {name: id, nickname: 'Nick'}); return ''; }
export function writeRenamed(ctx, id) { ctx.db.put('fighters', id, {name: id + '2'}); return ''; }
export function writeDisplay(ctx, id) { ctx.db.put('fighters', id, {name: id, displayName: 'Dee'}); return ''; }
";
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
        source: source.into(),
        tables: migrations.last().unwrap().schema.clone(),
        functions: [
            function("read", FunctionKind::Query),
            function("writeName", FunctionKind::Mutation),
            function("writeNickname", FunctionKind::Mutation),
            function("writeRenamed", FunctionKind::Mutation),
            function("writeDisplay", FunctionKind::Mutation),
        ]
        .into(),
    }
}

#[tokio::test]
async fn writers_that_predate_a_removed_optional_field_still_synchronize() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::new(
        "local".into(),
        Box::new(SqliteStore::open(directory.path().join("nicknames.db"), "local").unwrap()),
    )
    .unwrap();
    for (id, length) in [("a", 1), ("b", 2), ("c", 3)] {
        backend.deploy(nicknames(id, length)).await.unwrap();
    }
    write(&backend, "a", "writeName", "ann").await;
    assert_eq!(read(&backend, "c", "ann").await, json!({"name": "ann", "displayName": "ann"}));
    write(&backend, "b", "writeNickname", "bob").await;
    assert_eq!(read(&backend, "c", "bob").await, json!({"name": "bob", "displayName": "Nick"}));
}

#[tokio::test]
async fn a_write_runs_one_direction_so_the_derived_field_follows_the_name() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("local".into(), Box::new(SqliteStore::open(directory.path().join("once.db"), "local").unwrap()))
            .unwrap();
    for (id, length) in [("a", 1), ("b", 2), ("c", 3)] {
        backend.deploy(nicknames(id, length)).await.unwrap();
    }
    write(&backend, "a", "writeName", "ann").await;
    write(&backend, "a", "writeRenamed", "ann").await;
    assert_eq!(read(&backend, "c", "ann").await, json!({"name": "ann2", "displayName": "ann2"}));
    assert_eq!(read(&backend, "b", "ann").await, json!({"name": "ann2"}));
    write(&backend, "c", "writeDisplay", "dee").await;
    assert_eq!(read(&backend, "b", "dee").await, json!({"name": "dee", "nickname": "Dee"}));
}

#[tokio::test]
async fn a_slow_backfill_leaves_the_commit_lane_to_other_writes() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("local".into(), Box::new(SqliteStore::open(directory.path().join("slow.db"), "local").unwrap()))
            .unwrap();
    let spin = "displayName: (() => { let x = 0; for (let i = 0; i < 3e7; i++) x += i; return 'x' + (x < 0); })()";
    backend.deploy(transforming("old", 1, spin)).await.unwrap();
    write(&backend, "old", "seed", "0").await;
    let new = DeploymentId::new("new").unwrap();
    backend.install(transforming("new", 2, spin)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    write(&backend, "old", "writeOld", "live").await;
    assert!(!backend.readiness(new.clone()).await.unwrap().ready, "the write committed while the backfill ran");
    backend.ready(new).await.unwrap();
}

#[tokio::test]
async fn a_batch_cut_at_the_output_budget_commits_its_prefix_and_the_backfill_continues() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("local".into(), Box::new(SqliteStore::open(directory.path().join("budget.db"), "local").unwrap()))
            .unwrap();
    backend.deploy(deployment("old", 1)).await.unwrap();
    for id in ["a", "b", "c"] {
        write(&backend, "old", "writeLarge", id).await;
    }
    backend.deploy(deployment("new", 2)).await.unwrap();
    for id in ["a", "b", "c"] {
        let name = read(&backend, "new", id).await["displayName"].as_str().unwrap().len();
        assert_eq!(name, 400_000, "row {id} was transformed");
    }
}
