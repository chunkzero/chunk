use super::*;
use crate::{
    DocumentKey, Revision,
    tests::{commit, operation, write},
};
use serde_json::json;

fn open() -> (tempfile::TempDir, SqliteStore) {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    (directory, store)
}

#[test]
fn storage_contract() {
    let (_directory, mut store) = open();
    crate::tests::snapshots_preserve_point_and_empty_range_reads_across_atomic_changes(&mut store);
}

#[test]
fn a_failure_after_document_writes_rolls_back_documents_revision_and_outcome() {
    let (_directory, mut store) = open();
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER fail_outcome BEFORE INSERT ON outcomes BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        )
        .unwrap();
    assert!(
        store
            .commit(commit(
                "failed",
                0,
                vec![write("a", Some(json!(1))), write("b", Some(json!(2)))]
            ))
            .is_err()
    );
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, Revision(0));
    assert!(snapshot.get(&DocumentKey::new("profiles", "a").unwrap()).is_none());
    assert!(store.outcome(&operation("failed")).unwrap().is_none());
    store.connection.execute_batch("DROP TRIGGER fail_outcome;").unwrap();
    assert_eq!(
        store
            .commit(commit("failed", 0, vec![write("a", Some(json!(1)))]))
            .unwrap()
            .revision,
        Revision(1)
    );
}

#[test]
fn restart_recovers_unknown_outcomes_and_excludes_another_environment_writer() {
    let (directory, mut store) = open();
    let path = directory.path().join("data.db");
    assert!(matches!(SqliteStore::open(&path, "local"), Err(Error::WriterLocked)));
    let outcome = store
        .commit(commit(
            "lost-reply",
            0,
            vec![write("player", Some(json!({"coins": 4})))],
        ))
        .unwrap();
    drop(store);
    assert!(matches!(
        SqliteStore::open(&path, "other"),
        Err(Error::EnvironmentMismatch)
    ));
    let mut store = SqliteStore::open(&path, "local").unwrap();
    assert_eq!(store.outcome(&operation("lost-reply")).unwrap(), Some(outcome.clone()));
    assert_eq!(store.commit(commit("lost-reply", 0, vec![])).unwrap(), outcome);
    assert_eq!(store.snapshot().unwrap().revision, Revision(1));
    let mismatched = Operation {
        fingerprint: [8; 32],
        ..operation("lost-reply")
    };
    assert!(matches!(store.outcome(&mismatched), Err(Error::OperationMismatch)));
    assert_eq!(store.commit(commit("next", 1, vec![])).unwrap().revision, Revision(2));
}

#[test]
fn committed_wal_survives_exit_without_destructors() {
    const CHILD_PATH: &str = "CHUNK_STORE_CRASH_TEST_PATH";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        let mut store = SqliteStore::open(path, "local").unwrap();
        store
            .commit(commit(
                "crash-reply",
                0,
                vec![write("player", Some(json!({"coins": 9})))],
            ))
            .unwrap();
        // Exit skips connection close/checkpoint and simulates a lost commit reply.
        std::process::exit(0);
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("crash.db");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "sqlite::tests::committed_wal_survives_exit_without_destructors",
        ])
        .env(CHILD_PATH, &path)
        .status()
        .unwrap();
    assert!(status.success());
    let mut store = SqliteStore::open(path, "local").unwrap();
    let recovered = store.outcome(&operation("crash-reply")).unwrap().unwrap();
    assert_eq!(recovered.revision, Revision(1));
    assert_eq!(store.commit(commit("crash-reply", 0, vec![])).unwrap(), recovered);
    assert_eq!(
        store
            .snapshot()
            .unwrap()
            .get(&DocumentKey::new("profiles", "player").unwrap())
            .unwrap()
            .value,
        json!({"coins": 9})
    );
}
