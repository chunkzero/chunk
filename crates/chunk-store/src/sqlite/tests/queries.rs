use super::*;

#[test]
fn compound_indexes_use_sql_ranges_ordering_and_limits() {
    let (_directory, mut store) = open();
    let schema: DatabaseSchema = serde_json::from_value(json!({
        "matches": {
            "fields": {
                "player": {"schema": {"type": "string"}},
                "score": {"schema": {"type": "integer"}, "optional": true}
            },
            "indexes": {"by_player_score": ["player", "score"]}
        }
    }))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    let entries = [
        ("a", json!({"player": "alex", "score": 9})),
        ("c", json!({"player": "alex", "score": 2})),
        ("b", json!({"player": "alex", "score": 2})),
        ("missing", json!({"player": "alex"})),
        ("other", json!({"player": "sam", "score": 2})),
    ];
    let writes = entries
        .iter()
        .map(|(id, value)| crate::tests::write_to("matches", id, Some(value.clone())))
        .collect();
    store.commit(commit("scores", 2, writes)).unwrap();
    let snapshot = store.snapshot().unwrap();
    let mut range = IndexRange {
        table: "matches".into(),
        index: "by_player_score".into(),
        prefix: vec![json!("alex")],
        start: Some(json!(2)),
        end: Some(json!(9)),
        limit: 1,
    };
    assert_eq!(snapshot.scan_index(&range).unwrap()[0].0, "b");
    let (sql, params) = read::index_query(&schema["matches"], &range).unwrap();
    let plan: Vec<String> = store
        .connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map(rusqlite::params_from_iter(params), |row| row.get(3))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|step| step.contains("SEARCH") && step.contains(&schema::index_name("matches", "by_player_score"))),
        "{plan:?}"
    );
    assert!(!plan.iter().any(|step| step.contains("TEMP B-TREE")), "{plan:?}");
    range.limit = 10;
    let ids: Vec<_> = snapshot
        .scan_index(&range)
        .unwrap()
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    assert_eq!(ids, ["b", "c"]);
    range.start = None;
    range.end = Some(json!(2));
    assert_eq!(snapshot.scan_index(&range).unwrap()[0].0, "missing");
    range.end = None;
    range.prefix.push(json!(null));
    assert_eq!(snapshot.scan_index(&range).unwrap()[0].0, "missing");
    range.prefix[1] = json!("wrong type");
    assert!(snapshot.scan_index(&range).is_err());
}

#[test]
fn snapshots_decode_only_requested_rows() {
    let (_directory, mut store) = open();
    let schema: DatabaseSchema = serde_json::from_value(json!({
        "payloads": {"fields": {"items": {"schema": {"type": "array", "items": {"type": "integer"}}}}}
    }))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    store
        .commit(commit("healthy", 2, vec![write("a", Some(json!({"coins": 1})))]))
        .unwrap();
    // Fault injection: valid SQL/JSON but invalid application shape in an unrelated row.
    store
        .connection
        .execute(
            "INSERT INTO payloads (_id, _revision, _bytes, items) VALUES ('broken', 3, 2, '{}')",
            [],
        )
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        snapshot
            .get(&DocumentKey::new("profiles", "a").unwrap())
            .unwrap()
            .unwrap()
            .value,
        json!({"coins": 1})
    );
    assert_eq!(snapshot.scan_index(&by_coins()).unwrap().len(), 1);
    assert!(matches!(
        snapshot.get(&DocumentKey::new("payloads", "broken").unwrap()),
        Err(Error::Corrupt(_))
    ));
}
