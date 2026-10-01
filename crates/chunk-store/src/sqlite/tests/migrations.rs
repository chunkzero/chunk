use chunk_contract::{DatabaseSchema, Deployment, Migration, MigrationKind, MigrationTable};
use serde_json::{Value, json};

use super::*;

fn schema(field: &str) -> DatabaseSchema {
    serde_json::from_value(json!({"fighters": {"fields": {field: {"schema": {"type": "string"}}}}})).unwrap()
}

/// The journal of a rename of `fighters.name` to `displayName`, up to `length` entries.
fn journal(length: usize) -> Vec<Migration> {
    let entry = |id: &str, kind, finishes: Option<&str>, removed: &[&str], schema| Migration {
        id: id.into(),
        hash: "0".repeat(64),
        kind,
        finishes: finishes.map(Into::into),
        tables: if kind == MigrationKind::Baseline {
            std::collections::BTreeMap::new()
        } else {
            let added = if kind == MigrationKind::Expand { vec!["displayName".into()] } else { Vec::new() };
            let removed = removed.iter().map(ToString::to_string).collect();
            [("fighters".into(), MigrationTable { added, removed, back: kind == MigrationKind::Expand })].into()
        },
        schema,
    };
    let mut journal = vec![
        entry("0001_init", MigrationKind::Baseline, None, &[], schema("name")),
        entry("0002_rename", MigrationKind::Expand, None, &["name"], schema("displayName")),
        entry("0003_finish_rename", MigrationKind::Finish, Some("0002_rename"), &["name"], schema("displayName")),
    ];
    journal.truncate(length);
    journal
}

fn deployment(id: &str, length: usize) -> Deployment {
    let migrations = journal(length);
    Deployment {
        contracts: chunk_contract::Contracts { migrations: migrations.clone(), ..Default::default() },
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: "export {};".into(),
        tables: migrations.last().unwrap().schema.clone(),
        functions: std::collections::BTreeMap::new(),
    }
}

fn upper(rows: &[Value]) -> Vec<Value> {
    rows.iter().map(|row| json!({"displayName": row["name"].as_str().unwrap().to_uppercase()})).collect()
}

fn fighter(store: &mut SqliteStore, id: &str) -> Value {
    store.snapshot().unwrap().get(&DocumentKey::new("fighters", id).unwrap()).unwrap().unwrap().value
}

fn migration_error(result: Result<Revision>) -> String {
    match result {
        Err(Error::Migration(message)) => message,
        other => panic!("expected a migration error, got {other:?}"),
    }
}

/// A store with 300 fighters under the old shape and the rename installed, its backfill not yet run.
fn renamed(path: &std::path::Path) -> SqliteStore {
    let mut store = SqliteStore::open(path, "local").unwrap();
    let revision = store.install_deployment(&deployment("old", 1)).unwrap();
    let writes = (0..300)
        .map(|i| crate::tests::write_to("fighters", &format!("{i:03}"), Some(json!({"name": format!("n{i}")}))));
    store.commit(commit("seed", revision.0, writes.collect())).unwrap();
    store.install_deployment(&deployment("new", 2)).unwrap();
    store
}

#[test]
fn a_backfill_resumes_from_its_cursor_after_a_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let mut store = renamed(&path);
    let backfill = Work::Backfill { migration: "0002_rename".into(), table: "fighters".into() };
    let pending = store.pending_work().unwrap();
    assert_eq!((&pending[0].work, pending[0].done, pending[0].total), (&backfill, 0, 300));
    store.run_work(pending[0].id, &mut |_, _, rows| Ok(upper(&rows))).unwrap();
    assert_eq!(store.pending_work().unwrap()[0].done, 256);
    drop(store);

    let mut store = SqliteStore::open(&path, "local").unwrap();
    let mut seen = Vec::new();
    let id = store.pending_work().unwrap()[0].id;
    store
        .run_work(id, &mut |_, _, rows| {
            seen.extend(rows.iter().map(|row| row["_id"].clone()));
            Ok(upper(&rows))
        })
        .unwrap();
    assert_eq!((seen.len(), &seen[0]), (44, &json!("256")));
    assert!(store.pending_work().unwrap().is_empty());
    assert_eq!(fighter(&mut store, "299"), json!({"name": "n299", "displayName": "N299"}));
    assert_eq!(store.migrations().unwrap(), journal(2)[1..]);
}

#[test]
fn installs_need_the_applied_journal_and_roll_back_only_until_the_old_shape_is_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = renamed(&directory.path().join("data.db"));
    let id = store.pending_work().unwrap()[0].id;
    store.run_work(id, &mut |_, _, rows| Ok(upper(&rows))).unwrap();
    store.run_work(id, &mut |_, _, rows| Ok(upper(&rows))).unwrap();

    let mut edited = deployment("edited", 3);
    edited.contracts.migrations[1].hash = "1".repeat(64);
    assert!(migration_error(store.install_deployment(&edited)).contains("0002_rename differs"));
    store.install_deployment(&deployment("rollback", 1)).unwrap();

    store.install_deployment(&deployment("finished", 3)).unwrap();
    assert!(store.pending_work().unwrap().is_empty(), "resident deployments still declare fighters.name");
    store.release_deployment("old").unwrap();
    store.release_deployment("rollback").unwrap();
    let pending = store.pending_work().unwrap();
    assert_eq!(pending[0].work, Work::Drop { migration: "0002_rename".into() });
    store.run_work(pending[0].id, &mut |_, _, rows| Ok(upper(&rows))).unwrap();
    assert!(store.migrations().unwrap().is_empty());
    assert_eq!(fighter(&mut store, "007"), json!({"displayName": "N7"}));
    assert!(migration_error(store.install_deployment(&deployment("late", 1))).contains("fighters.name"));
}
