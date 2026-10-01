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
    assert!(migration_error(store.install_deployment(&deployment("late", 1))).contains("0002_rename"));
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

#[test]
fn an_empty_table_keeps_syncing_optional_additions_for_a_retained_writer() {
    let directory = tempfile::tempdir().unwrap();
    let mut history = journal(3);
    for entry in &mut history[1..] {
        entry.schema.get_mut("fighters").unwrap().fields.get_mut("displayName").unwrap().optional = true;
    }
    let mut store = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    store.install_deployment(&deployment("old", 1)).unwrap();
    store.install_deployment(&deployed("finished", &history)).unwrap();
    assert_eq!(store.migrations().unwrap().len(), 1, "the retained writer lacks the optional displayName");
}

/// A journal that expands `fighters` with the required integer `count` and finishes it.
fn counted() -> Vec<Migration> {
    let fields = |count: bool| -> DatabaseSchema {
        let mut fields = json!({"name": {"schema": {"type": "string"}}});
        if count {
            fields["count"] = json!({"schema": {"type": "integer"}});
        }
        serde_json::from_value(json!({"fighters": {"fields": fields}})).unwrap()
    };
    let entry = |id: &str, kind, finishes: Option<&str>, tables, schema| Migration {
        id: id.into(),
        hash: "0".repeat(64),
        kind,
        finishes: finishes.map(Into::into),
        tables,
        schema,
    };
    let change: std::collections::BTreeMap<_, _> =
        [("fighters".into(), MigrationTable { added: vec!["count".into()], removed: Vec::new(), back: false })].into();
    let finish: std::collections::BTreeMap<_, _> =
        [("fighters".into(), MigrationTable { added: Vec::new(), removed: Vec::new(), back: false })].into();
    vec![
        entry("0001_init", MigrationKind::Baseline, None, std::collections::BTreeMap::new(), fields(false)),
        entry("0002_count", MigrationKind::Expand, None, change.clone(), fields(true)),
        entry("0003_finish_count", MigrationKind::Finish, Some("0002_count"), finish, fields(true)),
    ]
}

#[test]
fn a_migration_stays_active_until_every_resident_deployment_has_the_new_shape() {
    let directory = tempfile::tempdir().unwrap();
    let mut removed = SqliteStore::open(directory.path().join("removed.db"), "local").unwrap();
    removed.install_deployment(&deployment("old", 1)).unwrap();
    removed.install_deployment(&deployment("finished", 3)).unwrap();
    assert_eq!(removed.migrations().unwrap().len(), 1, "the retained writer still declares fighters.name");
    let write = crate::tests::write_to("fighters", "a", Some(json!({"name": "a"})));
    let revision = removed.snapshot().unwrap().revision.0;
    removed.commit(commit("retained", revision, vec![write])).unwrap();

    let mut added = SqliteStore::open(directory.path().join("added.db"), "local").unwrap();
    let history = counted();
    added.install_deployment(&deployed("old", &history[..1])).unwrap();
    added.install_deployment(&deployed("finished", &history)).unwrap();
    assert_eq!(added.migrations().unwrap().len(), 1, "the retained writer lacks the required count");
    while let Some(pending) = added.pending_work().unwrap().first().cloned() {
        added.run_work(pending.id, &mut |_, _, _| Ok(json!({"count": 1}))).unwrap();
    }
    assert_eq!(added.migrations().unwrap().len(), 1, "the finished expand waits for the retained writer");
    added.release_deployment("old").unwrap();
    while let Some(pending) = added.pending_work().unwrap().first().cloned() {
        added.run_work(pending.id, &mut |_, _, _| Ok(json!({"count": 1}))).unwrap();
    }
    assert!(added.migrations().unwrap().is_empty());
}

#[test]
fn a_table_can_rename_a_field_with_the_most_fields_a_deployment_may_declare() {
    let directory = tempfile::tempdir().unwrap();
    let fields = |first: &str| -> DatabaseSchema {
        let mut fields: serde_json::Map<_, _> =
            (1..64).map(|i| (format!("f{i}"), json!({"schema": {"type": "string"}}))).collect();
        fields.insert(first.into(), json!({"schema": {"type": "string"}}));
        serde_json::from_value(json!({"fighters": {"fields": fields}})).unwrap()
    };
    let entry = |id: &str, kind, finishes: Option<&str>, schema| Migration {
        id: id.into(),
        hash: "0".repeat(64),
        kind,
        finishes: finishes.map(Into::into),
        tables: if kind == MigrationKind::Baseline {
            std::collections::BTreeMap::new()
        } else {
            [("fighters".into(), MigrationTable { added: vec!["g0".into()], removed: vec!["f0".into()], back: true })]
                .into()
        },
        schema,
    };
    let history = [
        entry("0001_init", MigrationKind::Baseline, None, fields("f0")),
        entry("0002_rename", MigrationKind::Expand, None, fields("g0")),
        entry("0003_finish_rename", MigrationKind::Finish, Some("0002_rename"), fields("g0")),
    ];
    let mut store = SqliteStore::open(directory.path().join("wide.db"), "local").unwrap();
    store.install_deployment(&deployed("old", &history[..1])).unwrap();
    store.install_deployment(&deployed("new", &history[..2])).unwrap();
    assert_eq!(store.migrations().unwrap().len(), 1);
    assert_eq!(store.schema.get("fighters").unwrap().fields.len(), 65);
    drop(store);
    SqliteStore::open(directory.path().join("wide.db"), "local").unwrap();
}

#[test]
fn a_backfill_refuses_values_a_read_could_not_return() {
    let directory = tempfile::tempdir().unwrap();
    let history = counted();
    let mut store = SqliteStore::open(directory.path().join("unsafe.db"), "local").unwrap();
    let revision = store.install_deployment(&deployed("old", &history[..1])).unwrap();
    let write = crate::tests::write_to("fighters", "a", Some(json!({"name": "a"})));
    store.commit(commit("seed", revision.0, vec![write])).unwrap();
    store.install_deployment(&deployed("new", &history[..2])).unwrap();
    let id = store.pending_work().unwrap()[0].id;
    let result = store.run_work(id, &mut |_, _, _| Ok(json!({"count": 9_007_199_254_740_992_i64})));
    assert!(matches!(&result, Err(Error::Migration(m)) if m.contains("invalid fighters row a")), "{result:?}");
    assert_eq!(store.pending_work().unwrap()[0].done, 0);
}

#[test]
fn a_backfill_skips_rows_written_since_it_read_them() {
    let directory = tempfile::tempdir().unwrap();
    let mut store = renamed(&directory.path().join("data.db"));
    let id = store.pending_work().unwrap()[0].id;
    let batch = store.read_backfill(id, 256).unwrap().unwrap();
    let revision = store.snapshot().unwrap().revision.0;
    let changed = crate::tests::write_to("fighters", "000", Some(json!({"name": "changed", "displayName": "SYNCED"})));
    store.commit(commit("write", revision, vec![changed])).unwrap();
    let outputs: Vec<_> = batch.rows.iter().map(|row| upper(&row.input)).collect();
    store.commit_backfill(id, &batch, &outputs).unwrap();
    assert_eq!(fighter(&mut store, "000"), json!({"name": "changed", "displayName": "SYNCED"}));
    assert_eq!(fighter(&mut store, "001")["displayName"], "N1");
    assert_eq!(store.pending_work().unwrap()[0].done, 256, "a skipped row still counts as passed");
}

#[test]
fn a_replicated_restore_recovers_a_partial_backfill_and_the_contraction_after_it() {
    use crate::replication::tests::{Memory, count, dump, manual, open, snapshotting};
    let storage = std::sync::Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let (mut store, replicator) = open(&path, snapshotting(&storage));
    let revision = store.install_deployment(&deployment("old", 1)).unwrap();
    replicator.flush().unwrap();
    let writes = (0..300)
        .map(|i| crate::tests::write_to("fighters", &format!("{i:03}"), Some(json!({"name": format!("n{i}")}))));
    store.commit(commit("seed", revision.0, writes.collect())).unwrap();
    replicator.flush().unwrap();
    store.install_deployment(&deployment("new", 2)).unwrap();
    let id = store.pending_work().unwrap()[0].id;
    store.run_work(id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    replicator.flush().unwrap();

    let partial = directory.path().join("partial.db");
    let (mut restored, _replicator) = open(&partial, manual(&storage.copy()));
    assert_eq!((restored.pending_work().unwrap()[0].done, restored.migrations().unwrap().len()), (256, 1));
    assert_eq!(fighter(&mut restored, "255")["displayName"], "N255");
    assert!(fighter(&mut restored, "256").get("displayName").is_none());
    drop(restored);
    assert_eq!(dump(&partial), dump(&path));

    store.run_work(id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    store.install_deployment(&deployment("finished", 3)).unwrap();
    for old in ["old", "new"] {
        store.release_deployment(old).unwrap();
    }
    let drop_id = store.pending_work().unwrap()[0].id;
    store.run_work(drop_id, &mut |_, _, row| Ok(upper(&row))).unwrap();
    replicator.flush().unwrap();

    assert!(count(&storage, "/snapshots/") > 1, "the restore starts from a snapshot taken mid-lifecycle");
    let contracted = directory.path().join("contracted.db");
    let (mut restored, _replicator) = open(&contracted, manual(&storage));
    assert!(restored.migrations().unwrap().is_empty() && restored.pending_work().unwrap().is_empty());
    assert_eq!(fighter(&mut restored, "299"), json!({"displayName": "N299"}));
    drop(restored);
    assert_eq!(dump(&contracted), dump(&path));
    let connection = rusqlite::Connection::open(&contracted).unwrap();
    let bytes = connection.query_row("SELECT document_bytes FROM _chunk_metadata", [], |row| row.get::<_, i64>(0));
    let expected: usize = (0..300).map(|i| json!({"displayName": format!("N{i}")}).to_string().len()).sum();
    assert_eq!(bytes.unwrap(), i64::try_from(expected).unwrap());
}

#[test]
fn an_older_writer_cannot_install_after_a_contraction() {
    let directory = tempfile::tempdir().unwrap();
    let mut optional = journal(3);
    for entry in &mut optional[1..] {
        entry.schema.get_mut("fighters").unwrap().fields.get_mut("displayName").unwrap().optional = true;
    }
    for (name, history) in [("required", counted()), ("renamed", optional)] {
        let mut store = SqliteStore::open(directory.path().join(format!("{name}.db")), "local").unwrap();
        store.install_deployment(&deployed("old", &history[..1])).unwrap();
        store.install_deployment(&deployed("finished", &history)).unwrap();
        store.release_deployment("old").unwrap();
        while let Some(pending) = store.pending_work().unwrap().first().cloned() {
            store.run_work(pending.id, &mut |_, _, _| Ok(json!({"count": 1, "displayName": "x"}))).unwrap();
        }
        assert!(store.migrations().unwrap().is_empty(), "{name} contracted");
        let late = migration_error(store.install_deployment(&deployed("late", &history[..1])));
        assert!(late.contains("0002_") && late.contains("no longer possible"), "{late}");
        store.install_deployment(&deployed("again", &history)).unwrap();
    }
}
