use super::*;
use crate::{
    DocumentKey, IndexRange,
    tests::{commit, operation, write},
};
use serde_json::json;

mod jobs;
mod queries;
mod retention;
mod schemas;
mod transactions;

fn open() -> (tempfile::TempDir, SqliteStore) {
    let directory = tempfile::tempdir().unwrap();
    let mut store = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    store.apply_schema(&crate::tests::schema()).unwrap();
    (directory, store)
}

fn totals(store: &SqliteStore) -> (usize, usize) {
    store
        .connection
        .query_row("SELECT document_count, document_bytes FROM _chunk_metadata", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap()
}

fn by_coins() -> IndexRange {
    IndexRange {
        table: "profiles".into(),
        index: "by_coins".into(),
        prefix: vec![],
        start: None,
        end: None,
        limit: 100,
    }
}

#[test]
fn storage_contract() {
    let (_directory, mut store) = open();
    crate::tests::snapshots_preserve_point_and_empty_range_reads_across_atomic_changes(&mut store);
}

#[test]
fn a_failure_after_document_writes_rolls_back_documents_indexes_counters_and_outcome() {
    let (_directory, mut store) = open();
    let schema = [("stats".into(), crate::tests::schema()["profiles"].clone())].into();
    store.apply_schema(&schema).unwrap();
    let stats = crate::tests::write_to("stats", "a", Some(json!({"coins": 2})));
    let context = RetryContext { deployment: "v1".into(), timestamp: 123, seed: 42 };
    store.prepare_operation(&operation("failed"), context.clone()).unwrap();
    store.connection.execute_batch("CREATE TRIGGER fail_outcome BEFORE INSERT ON _chunk_operations BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(store.commit(commit("failed", 2, vec![write("a", Some(json!({"coins": 1}))), stats.clone()])).is_err());
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, Revision(2));
    assert!(snapshot.scan_index(&by_coins()).unwrap().is_empty());
    assert!(snapshot.get(&DocumentKey::new("profiles", "a").unwrap()).unwrap().is_none());
    assert_eq!(totals(&store), (0, 0));
    assert!(snapshot.get(&stats.key).unwrap().is_none());
    assert!(snapshot.scan_index(&IndexRange { table: "stats".into(), ..by_coins() }).unwrap().is_empty());
    assert!(store.outcome(&operation("failed")).unwrap().is_none());
    assert_eq!(
        store.prepare_operation(&operation("failed"), RetryContext { seed: 99, ..context.clone() }).unwrap(),
        context
    );
    store.connection.execute_batch("DROP TRIGGER fail_outcome;").unwrap();
    assert_eq!(
        store.commit(commit("failed", 2, vec![write("a", Some(json!({"coins": 1}))), stats])).unwrap().revision,
        Revision(3)
    );
    assert_eq!(totals(&store), (2, 22));
    assert!(
        !store
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM _chunk_retry_contexts WHERE operation_id = ?1)",
                ["failed"],
                |row| row.get::<_, bool>(0),
            )
            .unwrap()
    );
}

#[test]
fn restart_recovers_unknown_outcomes_and_excludes_another_environment_writer() {
    let (directory, mut store) = open();
    let path = directory.path().join("data.db");
    assert!(matches!(SqliteStore::open(&path, "local"), Err(Error::WriterLocked)));
    let outcome = store.commit(commit("lost-reply", 1, vec![write("player", Some(json!({"coins": 4})))])).unwrap();
    drop(store);
    assert!(matches!(SqliteStore::open(&path, "other"), Err(Error::EnvironmentMismatch)));
    let mut store = SqliteStore::open(&path, "local").unwrap();
    assert_eq!(store.outcome(&operation("lost-reply")).unwrap(), Some(outcome.clone()));
    assert_eq!(store.commit(commit("lost-reply", 0, vec![])).unwrap(), outcome);
    assert_eq!(store.snapshot().unwrap().revision, Revision(2));
    assert_eq!(store.snapshot().unwrap().scan_index(&by_coins()).unwrap()[0].0, "player");
    let mismatched = Operation { fingerprint: [8; 32], ..operation("lost-reply") };
    assert!(matches!(store.outcome(&mismatched), Err(Error::OperationMismatch)));
    assert_eq!(store.commit(commit("next", 2, vec![])).unwrap().revision, Revision(3));
}

#[test]
fn committed_wal_survives_exit_without_destructors() {
    const CHILD_PATH: &str = "CHUNK_STORE_CRASH_TEST_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let mut store = SqliteStore::open(path, "local").unwrap();
        store.apply_schema(&crate::tests::schema()).unwrap();
        store.commit(commit("crash-reply", 1, vec![write("player", Some(json!({"coins": 9})))])).unwrap();
        // Exit skips connection close/checkpoint and simulates a lost commit reply.
        std::process::exit(0);
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("crash.db");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sqlite::tests::committed_wal_survives_exit_without_destructors"])
        .env(CHILD_PATH, &path)
        .status()
        .unwrap();
    assert!(status.success());
    let mut store = SqliteStore::open(path, "local").unwrap();
    let recovered = store.outcome(&operation("crash-reply")).unwrap().unwrap();
    assert_eq!(recovered.revision, Revision(2));
    assert_eq!(store.commit(commit("crash-reply", 0, vec![])).unwrap(), recovered);
    assert_eq!(
        store.snapshot().unwrap().get(&DocumentKey::new("profiles", "player").unwrap()).unwrap().unwrap().value,
        json!({"coins": 9})
    );
    assert_eq!(totals(&store), (1, 11));
}

#[test]
fn capacity_totals_follow_replacements_and_deletes() {
    let (_directory, mut store) = open();
    store
        .commit(commit("one", 1, vec![write("a", Some(json!({"coins": 1}))), write("b", Some(json!({"coins": 20})))]))
        .unwrap();
    assert_eq!(totals(&store), (2, 23));
    store
        .commit(commit(
            "two",
            2,
            vec![write("a", Some(json!({"coins": 100}))), write("b", None), write("missing", None)],
        ))
        .unwrap();
    assert_eq!(totals(&store), (1, 13));
    let full = write::MAX_DOCUMENT_TOTAL_BYTES - 13;
    store.connection.execute("UPDATE _chunk_metadata SET document_bytes = document_bytes + ?1", [full]).unwrap();
    assert!(matches!(
        store.commit(commit("too-large", 3, vec![write("b", Some(json!({"coins": 1})))])),
        Err(Error::Capacity)
    ));
    assert_eq!(totals(&store), (1, 13 + full));
    assert_eq!(store.snapshot().unwrap().revision, Revision(3));
    assert!(store.outcome(&operation("too-large")).unwrap().is_none());
}

#[test]
fn reused_read_connections_stay_pinned_while_held_and_see_later_commits() {
    let (_directory, mut store) = open();
    let key = DocumentKey::new("profiles", "alice").unwrap();
    let coins = |snapshot: &Snapshot| snapshot.get(&key).unwrap().map(|document| document.value["coins"].clone());
    drop(store.snapshot().unwrap());
    assert_eq!(store.readers.idle(), 1);

    store.commit(commit("one", 1, vec![write("alice", Some(json!({"coins": 1})))])).unwrap();
    let held = store.snapshot().unwrap();
    assert_eq!(store.readers.idle(), 0);
    store.commit(commit("two", 2, vec![write("alice", Some(json!({"coins": 2})))])).unwrap();
    let latest = store.snapshot().unwrap();
    assert_eq!((held.revision, coins(&held)), (Revision(2), Some(json!(1))));
    assert_eq!((latest.revision, coins(&latest)), (Revision(3), Some(json!(2))));

    drop((held, latest));
    assert_eq!(store.readers.idle(), 2);
    let reused = store.snapshot().unwrap();
    assert_eq!((reused.revision, coins(&reused)), (Revision(3), Some(json!(2))));
}
