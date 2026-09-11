use super::*;

#[test]
fn read_budgets_stop_before_decoding_and_accumulate_across_calls() {
    let (_directory, mut store) = open();
    store
        .commit(commit("rows", 1, vec![write("a", Some(json!({"coins": 1}))), write("b", Some(json!({"coins": 2})))]))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let mut budget = crate::ReadBudget::new(1, 1024);
    assert!(snapshot.get_bounded(&DocumentKey::new("profiles", "a").unwrap(), &mut budget).unwrap().is_some());
    assert!(matches!(
        snapshot.get_bounded(&DocumentKey::new("profiles", "b").unwrap(), &mut budget),
        Err(Error::ReadLimit)
    ));
    assert!(matches!(
        snapshot.scan_bounded(
            &crate::KeyRange { table: "profiles".into(), start: None, end: None },
            &mut crate::ReadBudget::new(1, 1024)
        ),
        Err(Error::ReadLimit)
    ));
    assert_eq!(
        snapshot
            .scan_index_bounded(&IndexRange { limit: 1, ..by_coins() }, &mut crate::ReadBudget::new(1, 1024))
            .unwrap()
            .len(),
        1
    );
    drop(snapshot);
    let schema = serde_json::from_value(
        json!({"payloads": {"fields": {"items": {"schema": {"type":"array", "items":{"type":"integer"}}}}}}),
    )
    .unwrap();
    store.apply_schema(&schema).unwrap();
    store.connection.execute("INSERT INTO payloads VALUES ('broken', 2, 2, '{}')", []).unwrap();
    let snapshot = store.snapshot().unwrap();
    let key = DocumentKey::new("payloads", "broken").unwrap();
    assert!(matches!(snapshot.get_bounded(&key, &mut crate::ReadBudget::new(1, 0)), Err(Error::ReadLimit)));
    assert!(matches!(snapshot.get(&key), Err(Error::Corrupt(_))));
}

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
    let writes = entries.iter().map(|(id, value)| crate::tests::write_to("matches", id, Some(value.clone()))).collect();
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
    let ids: Vec<_> = snapshot.scan_index(&range).unwrap().into_iter().map(|(id, _)| id).collect();
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
    store.commit(commit("healthy", 2, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    // Fault injection: valid SQL/JSON but invalid application shape in an unrelated row.
    store
        .connection
        .execute("INSERT INTO payloads (_id, _revision, _bytes, items) VALUES ('broken', 3, 2, '{}')", [])
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.get(&DocumentKey::new("profiles", "a").unwrap()).unwrap().unwrap().value, json!({"coins": 1}));
    assert_eq!(snapshot.scan_index(&by_coins()).unwrap().len(), 1);
    assert!(matches!(snapshot.get(&DocumentKey::new("payloads", "broken").unwrap()), Err(Error::Corrupt(_))));
}

#[test]
fn numeric_ranges_preserve_large_integer_and_fractional_bound_ordering() {
    let (_directory, mut store) = open();
    let schema = serde_json::from_value(json!({
        "numbers": {"fields": {"value": {"schema": {"type": "number"}}}, "indexes": {"by_value": ["value"]}}
    }))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    let values = [
        json!(i64::MIN),
        json!(-1.5),
        json!(-1),
        json!(0),
        json!(0.5),
        json!(9_007_199_254_740_992.0),
        json!(9_007_199_254_740_993_i64),
        json!(i64::MAX),
        json!(9_223_372_036_854_775_808.0),
    ];
    let writes = values
        .iter()
        .enumerate()
        .map(|(id, value)| crate::tests::write_to("numbers", &id.to_string(), Some(json!({"value": value}))))
        .collect();
    store.commit(commit("numbers", 2, writes)).unwrap();
    let snapshot = store.snapshot().unwrap();
    for (start, end, expected) in [
        (json!(-1.5), json!(-1), vec!["1"]),
        (json!(-1), json!(0.5), vec!["2", "3"]),
        (json!(9_007_199_254_740_992.0), json!(9_007_199_254_740_993_i64), vec!["5"]),
        (json!(i64::MAX), json!(9_223_372_036_854_775_808.0), vec!["7"]),
        (json!(i64::MIN), json!(-9_223_372_036_854_775_808.0), vec![]),
    ] {
        let range = IndexRange {
            table: "numbers".into(),
            index: "by_value".into(),
            prefix: vec![],
            start: Some(start),
            end: Some(end),
            limit: 100,
        };
        let actual: Vec<_> = snapshot.scan_index(&range).unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(actual, expected, "{range:?}");
        if !expected.is_empty() {
            let reversed = IndexRange { start: range.end, end: range.start, ..range };
            assert!(matches!(snapshot.scan_index(&reversed), Err(Error::Invalid(_))));
        }
    }
}

#[test]
fn optional_index_bounds_and_replacements_track_absence_without_stale_entries() {
    let (_directory, mut store) = open();
    let schema = serde_json::from_value(json!({
        "scores": {"fields": {"score": {"optional": true, "schema": {"type": "integer"}}}, "indexes": {"by_score": ["score"]}}
    })).unwrap();
    store.apply_schema(&schema).unwrap();
    store
        .commit(commit(
            "seed",
            2,
            vec![
                crate::tests::write_to("scores", "a", Some(json!({}))),
                crate::tests::write_to("scores", "b", Some(json!({"score": 1}))),
                crate::tests::write_to("scores", "c", Some(json!({"score": 2}))),
            ],
        ))
        .unwrap();
    let old = store.snapshot().unwrap();
    let range = IndexRange { table: "scores".into(), index: "by_score".into(), ..by_coins() };
    for (start, end, expected) in [
        (None, Some(json!(null)), vec![]),
        (Some(json!(null)), Some(json!(null)), vec![]),
        (Some(json!(null)), Some(json!(1)), vec!["a"]),
        (Some(json!(1)), Some(json!(1)), vec![]),
        (Some(json!(1)), None, vec!["b", "c"]),
    ] {
        let query = IndexRange { start, end, ..range.clone() };
        let ids: Vec<_> = old.scan_index(&query).unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, expected);
    }
    assert!(matches!(
        old.scan_index(&IndexRange { start: Some(json!(1)), end: Some(json!(null)), ..range.clone() }),
        Err(Error::Invalid(_))
    ));
    store
        .commit(commit(
            "replace",
            3,
            vec![
                crate::tests::write_to("scores", "a", Some(json!({"score": 3}))),
                crate::tests::write_to("scores", "b", Some(json!({}))),
                crate::tests::write_to("scores", "c", None),
            ],
        ))
        .unwrap();
    let current = store.snapshot().unwrap();
    let ids: Vec<_> = current.scan_index(&range).unwrap().into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids, ["b", "a"]);
    assert_eq!(old.scan_index(&range).unwrap().len(), 3);
    assert_eq!(current.scan_index(&IndexRange { prefix: vec![json!(null)], ..range }).unwrap()[0].0, "b");
}

#[test]
fn invalid_index_queries_are_rejected_even_for_empty_tables() {
    let (_directory, mut store) = open();
    let snapshot = store.snapshot().unwrap();
    for range in [
        IndexRange { limit: 0, ..by_coins() },
        IndexRange { limit: 100_001, ..by_coins() },
        IndexRange { table: "bad table".into(), ..by_coins() },
        IndexRange { index: "absent".into(), ..by_coins() },
        IndexRange { prefix: vec![json!(1), json!(2)], ..by_coins() },
        IndexRange { prefix: vec![json!(1)], start: Some(json!(2)), ..by_coins() },
        IndexRange { prefix: vec![json!(null)], ..by_coins() },
        IndexRange { start: Some(json!("1")), ..by_coins() },
        IndexRange { start: Some(json!(2)), end: Some(json!(1)), ..by_coins() },
        IndexRange { end: Some(json!(null)), ..by_coins() },
    ] {
        assert!(matches!(range.validate(&crate::tests::schema()["profiles"]), Err(Error::Invalid(_))), "{range:?}");
        assert!(matches!(snapshot.scan_index(&range), Err(Error::Invalid(_))), "{range:?}");
    }
}

#[test]
fn primary_key_scans_use_half_open_lexical_bounds() {
    let (_directory, mut store) = open();
    store
        .commit(commit(
            "keys",
            1,
            ["a", "aa", "b", "é"].into_iter().map(|id| write(id, Some(json!({"coins": 1})))).collect(),
        ))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    for (start, end, expected) in [
        (None, None, vec!["a", "aa", "b", "é"]),
        (Some("a"), Some("b"), vec!["a", "aa"]),
        (Some("aa"), Some("aa"), vec![]),
        (Some("b"), None, vec!["b", "é"]),
        (None, Some("a"), vec![]),
    ] {
        let range =
            crate::KeyRange { table: "profiles".into(), start: start.map(str::to_owned), end: end.map(str::to_owned) };
        let ids: Vec<_> = snapshot.scan(&range).unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, expected);
    }
    assert!(matches!(
        snapshot.scan(&crate::KeyRange { table: "profiles".into(), start: Some("z".into()), end: Some("a".into()) }),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn boolean_and_string_index_bounds_follow_declared_scalar_order() {
    let (_directory, mut store) = open();
    let schema = serde_json::from_value(json!({
        "flags": {"fields": {"active": {"schema": {"type": "boolean"}}, "name": {"schema": {"type": "string"}}}, "indexes": {"by_active_name": ["active", "name"]}}
    })).unwrap();
    store.apply_schema(&schema).unwrap();
    store
        .commit(commit(
            "flags",
            2,
            [("a", false, "alpha"), ("b", true, "alpha"), ("c", true, "beta"), ("d", true, "é")]
                .into_iter()
                .map(|(id, active, name)| {
                    crate::tests::write_to("flags", id, Some(json!({"active": active, "name": name})))
                })
                .collect(),
        ))
        .unwrap();
    let snapshot = store.snapshot().unwrap();
    let range = IndexRange {
        table: "flags".into(),
        index: "by_active_name".into(),
        start: Some(json!(false)),
        end: Some(json!(true)),
        ..by_coins()
    };
    assert_eq!(snapshot.scan_index(&range).unwrap()[0].0, "a");
    let names =
        IndexRange { prefix: vec![json!(true)], start: Some(json!("beta")), end: Some(json!("é")), ..range.clone() };
    assert_eq!(snapshot.scan_index(&names).unwrap()[0].0, "c");
    assert!(matches!(
        snapshot.scan_index(&IndexRange { start: Some(json!(true)), end: Some(json!(false)), ..range }),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        snapshot.scan_index(&IndexRange { start: Some(json!("é")), end: Some(json!("beta")), ..names }),
        Err(Error::Invalid(_))
    ));
}
