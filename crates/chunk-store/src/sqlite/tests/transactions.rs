use super::*;
use crate::tests::{committed, request, write_to};

#[test]
fn replay_returns_original_outcome_before_validating_or_applying_new_payload() {
    let (_directory, mut store) = open();
    let original = store.commit(commit("once", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    store.commit(commit("later", 2, vec![write("a", Some(json!({"coins": 2})))])).unwrap();
    let mut replay = commit("once", 0, vec![write_to("unknown", "bad", Some(json!(false)))]);
    replay.result = json!("different result");
    assert_eq!(store.commit(replay).unwrap(), original);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.revision, Revision(3));
    assert_eq!(snapshot.scan_index(&by_coins()).unwrap()[0].1.value, json!({"coins": 2}));
    let mut mismatch = commit("once", 3, vec![write("a", None)]);
    mismatch.operation.fingerprint = [8; 32];
    assert!(matches!(store.commit(mismatch), Err(Error::OperationMismatch)));
    assert_eq!(store.snapshot().unwrap().revision, Revision(3));
    assert_eq!(totals(&store), (1, 11));
}

#[test]
fn schema_revision_conflicts_are_retryable_without_consuming_operation_identity() {
    let (_directory, mut store) = open();
    let old = store.snapshot().unwrap();
    let addition = [("stats".into(), crate::tests::schema()["profiles"].clone())].into();
    store.apply_schema(&addition).unwrap();
    assert!(matches!(
        store.commit(commit("retry", old.revision.0, vec![write("a", Some(json!({"coins": 5})))])),
        Err(Error::Conflict { expected: Revision(1), actual: Revision(2) })
    ));
    assert!(store.outcome(&operation("retry")).unwrap().is_none());
    assert_eq!(totals(&store), (0, 0));
    assert_eq!(
        store.commit(commit("retry", 2, vec![write("a", Some(json!({"coins": 5})))])).unwrap().revision,
        Revision(3)
    );
    assert!(old.scan_index(&by_coins()).unwrap().is_empty());
}

#[test]
fn invalid_batch_members_leave_valid_members_and_operation_uncommitted() {
    let (_directory, mut store) = open();
    let mut invalid_key = write("bad", None);
    invalid_key.key.id = "bad\0id".into();
    for invalid in [
        write("bad", Some(json!({"coins": "wrong"}))),
        write("bad", Some(json!([]))),
        write_to("absent", "bad", None),
        invalid_key,
        write("a", None),
    ] {
        assert!(matches!(
            store.commit(commit("retry", 1, vec![write("a", Some(json!({"coins": 1}))), invalid])),
            Err(Error::Invalid(_))
        ));
        assert_eq!(store.snapshot().unwrap().revision, Revision(1));
        assert!(store.snapshot().unwrap().scan_index(&by_coins()).unwrap().is_empty());
        assert!(store.outcome(&operation("retry")).unwrap().is_none());
        assert_eq!(totals(&store), (0, 0));
    }
    store.commit(commit("retry", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
}

#[test]
fn commit_byte_budget_and_result_size_limits_reject_atomically_at_the_boundary() {
    let (_directory, mut store) = open();
    let schema =
        serde_json::from_value(json!({"large": {"fields": {"text": {"schema": {"type": "string"}}}}})).unwrap();
    store.apply_schema(&schema).unwrap();
    let value = json!({"text": "x".repeat(1024 * 1024 - 16)});
    let writes = (0..65).map(|id| write_to("large", &id.to_string(), Some(value.clone()))).collect();
    assert!(matches!(store.commit(commit("batch", 2, writes)), Err(Error::Capacity)));
    let mut oversized = commit("batch", 2, vec![write("a", Some(json!({"coins": 1})))]);
    oversized.result = json!("x".repeat(1024 * 1024));
    assert!(matches!(store.commit(oversized), Err(Error::Capacity)));
    assert_eq!(totals(&store), (0, 0));
    assert!(store.outcome(&operation("batch")).unwrap().is_none());
    let writes = (0..10_000).map(|id| write(&id.to_string(), Some(json!({"coins": id})))).collect();
    let mut allowed = commit("batch", 2, writes);
    allowed.result = json!("x".repeat(1024 * 1024 - 2));
    let result = store.commit(allowed).unwrap();
    assert_eq!(result.revision, Revision(3));
    assert_eq!(store.outcome(&operation("batch")).unwrap(), Some(result));
    assert_eq!(totals(&store).0, 10_000);
}

#[test]
fn document_byte_limit_accepts_exact_boundary_and_rejects_replacement_overflow() {
    let (_directory, mut store) = open();
    let schema = serde_json::from_value(json!({
        "large": {"fields": {"text": {"schema": {"type": "string"}}}}
    }))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    let overhead = serde_json::to_vec(&json!({"text": ""})).unwrap().len();
    let value = json!({"text": "x".repeat(1024 * 1024 - overhead)});
    store.commit(commit("exact", 2, vec![write_to("large", "a", Some(value.clone()))])).unwrap();
    let oversized = json!({"text": "x".repeat(1024 * 1024 - overhead + 1)});
    assert!(matches!(
        store.commit(commit("overflow", 3, vec![write_to("large", "a", Some(oversized))])),
        Err(Error::Capacity)
    ));
    assert_eq!(totals(&store), (1, 1024 * 1024));
    assert_eq!(store.snapshot().unwrap().get(&DocumentKey::new("large", "a").unwrap()).unwrap().unwrap().value, value);
    assert!(store.outcome(&operation("overflow")).unwrap().is_none());
}

#[test]
fn empty_and_missing_delete_commits_advance_revision_and_remain_replayable() {
    let (_directory, mut store) = open();
    let empty = store.commit(commit("empty", 1, vec![])).unwrap();
    let deleted = store.commit(commit("delete", 2, vec![write("missing", None)])).unwrap();
    assert_eq!(empty.revision, Revision(2));
    assert_eq!(deleted.revision, Revision(3));
    assert_eq!(store.commit(commit("empty", 0, vec![])).unwrap(), empty);
    assert_eq!(store.outcome(&operation("delete")).unwrap(), Some(deleted));
    assert_eq!(totals(&store), (0, 0));
}

#[test]
fn cloned_snapshots_outlive_the_writer_and_can_be_read_on_another_thread() {
    let (directory, mut store) = open();
    store.commit(commit("seed", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    let snapshot = store.snapshot().unwrap();
    let clone = snapshot.clone();
    drop(store);
    let mut reopened = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    reopened.commit(commit("delete", 2, vec![write("a", None)])).unwrap();
    let documents = std::thread::spawn(move || clone.scan_index(&by_coins()).unwrap()).join().unwrap();
    assert_eq!(documents[0].1.value, json!({"coins": 1}));
    assert_eq!(snapshot.scan_index(&by_coins()).unwrap(), documents);
    assert!(reopened.snapshot().unwrap().scan_index(&by_coins()).unwrap().is_empty());
}

#[test]
fn a_batch_keeps_each_commit_separate_and_undoes_only_rejected_ones() {
    let (_directory, mut store) = open();
    let rejected = crate::JobIntent::Cancel { id: "missing".into(), caller: json!(null) };
    let context = RetryContext { deployment: "v1".into(), timestamp: 1, seed: 2 };
    let results = store.batch(vec![
        request(commit("first", 1, vec![write("a", Some(json!({"coins": 1})))]), vec![]),
        crate::Request::Prepare { operation: operation("later"), context: context.clone() },
        // Rejected by its job intent after its document write ran.
        request(commit("rejected", 2, vec![write("b", Some(json!({"coins": 2})))]), vec![rejected]),
        request(commit("second", 2, vec![write("c", Some(json!({"coins": 3})))]), vec![]),
    ]);
    assert_eq!(results.len(), 4);
    assert_eq!(committed(&results[0]), Revision(2));
    assert!(matches!(&results[1], Ok(crate::Reply::Prepared(prepared)) if *prepared == context));
    assert!(matches!(results[2], Err(Error::Invalid(_))));
    assert_eq!(committed(&results[3]), Revision(3));
    assert_eq!(
        store.prepare_operation(&operation("later"), RetryContext { seed: 9, ..context.clone() }).unwrap(),
        context
    );
    assert!(store.snapshot().unwrap().get(&DocumentKey::new("profiles", "b").unwrap()).unwrap().is_none());
    assert_eq!(totals(&store), (2, 22));
    assert!(store.outcome(&operation("rejected")).unwrap().is_none());
    assert_eq!(store.outcome(&operation("second")).unwrap().unwrap().revision, Revision(3));
}

#[test]
fn a_storage_failure_undoes_the_whole_batch() {
    let (_directory, mut store) = open();
    store
        .connection
        .execute_batch("CREATE TRIGGER fail BEFORE INSERT ON _chunk_operations WHEN NEW.operation_id = 'second' BEGIN SELECT RAISE(ABORT, 'injected'); END;")
        .unwrap();
    let results = store.batch(vec![
        request(commit("first", 1, vec![write("a", Some(json!({"coins": 1})))]), vec![]),
        request(commit("second", 2, vec![]), vec![]),
        request(commit("third", 3, vec![]), vec![]),
    ]);
    assert!(matches!(results.as_slice(), [Err(error)] if !error.rejected()));
    assert!(store.outcome(&operation("first")).unwrap().is_none());
    assert_eq!(store.snapshot().unwrap().revision, Revision(1));
    assert_eq!(totals(&store), (0, 0));
}

#[test]
#[ignore = "measures fsync throughput"]
fn group_commit_throughput() {
    const COMMITS: u32 = 2048;
    for size in [1, 4, 16, 64] {
        let (_directory, mut store) = open();
        let started = std::time::Instant::now();
        for round in 0..COMMITS / size {
            let batch = (0..size)
                .map(|index| {
                    let number = round * size + index;
                    let writes = vec![write(&number.to_string(), Some(json!({"coins": number})))];
                    request(commit(&number.to_string(), u64::from(number) + 1, writes), Vec::new())
                })
                .collect();
            assert!(store.batch(batch).iter().all(Result::is_ok));
        }
        let rate = f64::from(COMMITS) / started.elapsed().as_secs_f64();
        println!("batches of {size}: {rate:.0} commits/s");
    }
}
