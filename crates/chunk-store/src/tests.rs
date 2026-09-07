use crate::{Commit, DatabaseSchema, DocumentKey, Error, IndexRange, KeyRange, Operation, Revision, Storage, Write};
use serde_json::json;

pub(crate) fn schema() -> DatabaseSchema {
    serde_json::from_value(json!({
        "profiles": {
            "fields": {"coins": {"schema": {"type": "integer"}}},
            "indexes": {"by_coins": ["coins"]}
        }
    }))
    .unwrap()
}

pub(crate) fn operation(id: &str) -> Operation {
    Operation {
        id: id.into(),
        fingerprint: [7; 32],
    }
}

pub(crate) fn write(id: &str, value: Option<serde_json::Value>) -> Write {
    Write {
        key: DocumentKey::new("profiles", id).unwrap(),
        value,
    }
}

pub(crate) fn commit(id: &str, revision: u64, writes: Vec<Write>) -> Commit {
    Commit {
        expected: Revision(revision),
        operation: operation(id),
        writes,
        result: json!({"committed": id}),
    }
}

pub(crate) fn snapshots_preserve_point_and_empty_range_reads_across_atomic_changes(store: &mut impl Storage) {
    let base = store.apply_schema(&schema()).unwrap().0;
    let empty = store.snapshot().unwrap();
    let range = KeyRange {
        table: "profiles".into(),
        start: Some("b".into()),
        end: Some("d".into()),
    };
    let index = IndexRange {
        table: "profiles".into(),
        index: "by_coins".into(),
        prefix: vec![],
        start: Some(json!(2)),
        end: Some(json!(4)),
        limit: 10,
    };
    let key = DocumentKey::new("profiles", "c").unwrap();
    assert!(empty.scan(&range).unwrap().is_empty());
    assert!(empty.scan_index(&index).unwrap().is_empty());
    assert!(empty.get(&key).unwrap().is_none());
    store
        .commit(commit(
            "one",
            base,
            vec![
                write("a", Some(json!({"coins": 1}))),
                write("c", Some(json!({"coins": 2}))),
            ],
        ))
        .unwrap();
    let first = store.snapshot().unwrap();
    assert_eq!(first.revision, Revision(base + 1));
    store
        .commit(commit(
            "two",
            base + 1,
            vec![write("c", None), write("b", Some(json!({"coins": 3})))],
        ))
        .unwrap();
    let second = store.snapshot().unwrap();
    assert_eq!(first.scan(&range).unwrap()[0].0, "c");
    assert_eq!(second.scan(&range).unwrap()[0].0, "b");
    assert_eq!(first.scan_index(&index).unwrap()[0].0, "c");
    assert_eq!(second.scan_index(&index).unwrap()[0].0, "b");
    assert!(empty.scan(&range).unwrap().is_empty());
    assert!(empty.scan_index(&index).unwrap().is_empty());
    assert!(empty.get(&key).unwrap().is_none());
    assert_eq!(first.get(&key).unwrap().unwrap().value, json!({"coins": 2}));
    assert!(second.get(&key).unwrap().is_none());
    assert_eq!(
        second
            .get(&DocumentKey::new("profiles", "a").unwrap())
            .unwrap()
            .unwrap()
            .revision,
        Revision(base + 1)
    );
    assert!(matches!(
        store.commit(commit("stale", base + 1, vec![])),
        Err(Error::Conflict { .. })
    ));
    assert!(store.outcome(&operation("stale")).unwrap().is_none());
}
