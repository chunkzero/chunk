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
    journal.push(Migration {
        id: "0004_title".into(),
        hash: "0".repeat(64),
        kind: MigrationKind::Expand,
        finishes: None,
        tables: [(
            "fighters".into(),
            MigrationTable { added: vec!["title".into()], removed: vec!["displayName".into()], back: true },
        )]
        .into(),
        schema: schema("title"),
    });
    journal.truncate(length);
    journal
}

fn deployment(id: &str, length: usize) -> Deployment {
    deployed(id, &journal(length))
}

fn deployed(id: &str, migrations: &[Migration]) -> Deployment {
    Deployment {
        contracts: chunk_contract::Contracts { migrations: migrations.to_owned(), ..Default::default() },
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: chunk_contract::RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: "export {};".into(),
        tables: migrations.last().unwrap().schema.clone(),
        functions: std::collections::BTreeMap::new(),
    }
}

fn upper(row: &Value) -> Value {
    json!({"displayName": row["name"].as_str().unwrap().to_uppercase()})
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
    store.run_work(pending[0].id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    assert_eq!(store.pending_work().unwrap()[0].done, 256);
    drop(store);

    let mut store = SqliteStore::open(&path, "local").unwrap();
    let mut seen = Vec::new();
    let id = store.pending_work().unwrap()[0].id;
    store
        .run_work(id, &mut |_, _, row| {
            seen.push(row["_id"].clone());
            Ok(upper(&row))
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
    store.run_work(id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    store.run_work(id, &mut |_, _, row| Ok(upper(&row))).unwrap();

    let mut edited = deployment("edited", 3);
    edited.contracts.migrations[1].hash = "1".repeat(64);
    assert!(migration_error(store.install_deployment(&edited)).contains("0002_rename differs"));
    store.install_deployment(&deployment("rollback", 1)).unwrap();

    store.install_deployment(&deployment("finished", 3)).unwrap();
    assert!(store.pending_work().unwrap().is_empty(), "resident deployments still declare fighters.name");
    let chained = migration_error(store.install_deployment(&deployment("chained", 4)));
    assert!(chained.contains("0004_title") && chained.contains("intermediate release"), "{chained}");
    let mut baseline = journal(3)[..1].to_vec();
    baseline[0].id = "0003_base".into();
    baseline[0].schema = schema("displayName");
    let early = migration_error(store.install_deployment(&deployed("baseline", &baseline)));
    assert!(early.contains("0003_base") && early.contains("hasn't completed"), "{early}");
    store.release_deployment("old").unwrap();
    store.release_deployment("rollback").unwrap();
    let pending = store.pending_work().unwrap();
    assert_eq!(pending[0].work, Work::Drop { migration: "0002_rename".into() });
    store.run_work(pending[0].id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    assert!(store.migrations().unwrap().is_empty());
    assert_eq!(fighter(&mut store, "007"), json!({"displayName": "N7"}));
    assert!(migration_error(store.install_deployment(&deployment("late", 1))).contains("fighters.name"));
    store.install_deployment(&deployed("baseline", &baseline)).unwrap();
}

#[test]
fn a_failing_transform_names_its_migration_and_row() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = renamed(&directory.path().join("data.db"));
    let id = store.pending_work().unwrap()[0].id;
    let result = store.run_work(id, &mut |_, _, row| {
        if row["_id"] == "003" { Err("boom".into()) } else { Ok(upper(&row)) }
    });
    assert!(matches!(&result, Err(Error::Migration(m)) if m.contains("0002_rename") && m.contains("row 003")));
    assert_eq!(store.pending_work().unwrap()[0].done, 0, "a failed batch commits nothing");
}

#[test]
fn a_drop_waits_for_a_pending_backfill_and_recounts_the_remaining_documents() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let mut store = renamed(&path);
    let settle = |store: &mut SqliteStore| {
        while let Some(pending) = store.pending_work().unwrap().first().cloned() {
            store.run_work(pending.id, &mut |_, _, row| Ok(upper(&row))).unwrap();
        }
    };
    settle(&mut store);
    store.install_deployment(&deployment("finished", 3)).unwrap();
    for id in ["old", "new"] {
        store.release_deployment(id).unwrap();
    }
    assert_eq!(store.pending_work().unwrap()[0].work, Work::Drop { migration: "0002_rename".into() });
    store.release_deployment("finished").unwrap();
    store.install_deployment(&deployment("again", 2)).unwrap();
    let pending = store.pending_work().unwrap();
    assert!(matches!(pending[1].work, Work::Backfill { .. }));
    store.run_work(pending[0].id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    assert_eq!(store.migrations().unwrap().len(), 1, "the drop waits for the backfill");

    settle(&mut store);
    assert!(store.migrations().unwrap().is_empty());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let bytes = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
    assert_eq!(bytes("SELECT _bytes FROM fighters WHERE _id = '007'"), 20);
    let expected: usize = (0..300).map(|i| json!({"displayName": format!("N{i}")}).to_string().len()).sum();
    assert_eq!(bytes("SELECT document_bytes FROM _chunk_metadata"), i64::try_from(expected).unwrap());
}

#[test]
fn a_backfill_without_a_carrier_restarts_from_the_first_row() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = renamed(&directory.path().join("data.db"));
    let id = store.pending_work().unwrap()[0].id;
    store.run_work(id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    assert_eq!(store.pending_work().unwrap()[0].done, 256);
    store.release_deployment("new").unwrap();
    let revision = store.snapshot().unwrap().revision.0;
    let changed = crate::tests::write_to("fighters", "000", Some(json!({"name": "changed"})));
    store.commit(commit("gap", revision, vec![changed])).unwrap();

    store.install_deployment(&deployment("again", 2)).unwrap();
    assert_eq!(store.pending_work().unwrap()[0].done, 0);
    while let Some(pending) = store.pending_work().unwrap().first().cloned() {
        store.run_work(pending.id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    }
    assert_eq!(fighter(&mut store, "000")["displayName"], "CHANGED");
}

#[test]
fn an_empty_table_applies_finished_expands_directly_and_a_populated_one_needs_an_intermediate_release() {
    let directory = tempfile::tempdir().unwrap();
    let history = journal(4);
    let mut empty = SqliteStore::open(directory.path().join("empty.db"), "local").unwrap();
    empty.install_deployment(&deployed("fresh", &history)).unwrap();
    let pending = empty.pending_work().unwrap();
    let title = Work::Backfill { migration: "0004_title".into(), table: "fighters".into() };
    assert_eq!(pending.iter().map(|pending| &pending.work).collect::<Vec<_>>(), [&title], "no work for the rename");
    assert_eq!(empty.migrations().unwrap(), history[3..], "only the open expand stays active");

    let mut populated = SqliteStore::open(directory.path().join("populated.db"), "local").unwrap();
    let revision = populated.install_deployment(&deployment("old", 1)).unwrap();
    let write = crate::tests::write_to("fighters", "a", Some(json!({"name": "a"})));
    populated.commit(commit("seed", revision.0, vec![write])).unwrap();
    let error = migration_error(populated.install_deployment(&deployed("fresh", &history)));
    assert!(error.contains("0004_title") && error.contains("intermediate release"), "{error}");
}
