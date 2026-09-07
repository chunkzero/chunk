use crate::{Commit, DocumentKey, Error, KeyRange, Operation, Revision, Storage, Write};
use serde_json::json;

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
    let empty = store.snapshot().unwrap();
    let range = KeyRange {
        table: "profiles".into(),
        start: Some("b".into()),
        end: Some("d".into()),
    };
    assert!(empty.scan(&range).unwrap().is_empty());
    store
        .commit(commit(
            "one",
            0,
            vec![write("a", Some(json!(1))), write("c", Some(json!(2)))],
        ))
        .unwrap();
    let first = store.snapshot().unwrap();
    assert_eq!(first.revision, Revision(1));
    assert_eq!(first.scan(&range).unwrap()[0].0, "c");
    assert!(empty.scan(&range).unwrap().is_empty());
    store
        .commit(commit("two", 1, vec![write("c", None), write("b", Some(json!(3)))]))
        .unwrap();
    let second = store.snapshot().unwrap();
    assert_eq!(second.scan(&range).unwrap()[0].0, "b");
    assert_eq!(first.scan(&range).unwrap()[0].0, "c");
    assert_eq!(
        second
            .get(&DocumentKey::new("profiles", "a").unwrap())
            .unwrap()
            .revision,
        Revision(1)
    );
    assert!(matches!(
        store.commit(commit("stale", 1, vec![])),
        Err(Error::Conflict { .. })
    ));
    assert!(store.outcome(&operation("stale")).unwrap().is_none());
}
