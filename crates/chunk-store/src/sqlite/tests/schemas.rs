use super::*;
use chunk_contract::{Field, Schema, TableSchema};

#[test]
fn physical_columns_round_trip_scalars_json_and_absence() {
    let (_directory, mut store) = open();
    let schema: DatabaseSchema = serde_json::from_value(json!({
        "matches": {
            "fields": {
                "session": {"schema": {"type": "session"}},
                "player": {"schema": {"type": "player"}, "optional": true},
                "profile": {"schema": {"type": "id", "table": "profiles"}, "optional": true},
                "active": {"schema": {"type": "boolean"}},
                "score": {"schema": {"type": "integer"}},
                "rating": {"schema": {"type": "number"}},
                "result": {"optional": true, "schema": {"type": "union", "variants": [
                    {"type": "null"},
                    {"type": "object", "fields": {"winner": {"schema": {"type": "string"}}}}
                ]}},
                "rounds": {"schema": {"type": "array", "items": {"type": "integer"}}}
            },
            "indexes": {"by_session": ["session"]}
        }
    }))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    let values = [
        json!({"session": "s", "player": "alex", "profile": "profiles:p1", "active": true, "score": i64::MAX, "rating": 9_007_199_254_740_993_i64, "rounds": [1, 2]}),
        json!({"session": "s", "active": false, "score": i64::MIN, "rating": 1.5, "rounds": [], "result": null}),
        json!({"session": "s", "active": false, "score": 0, "rating": 1.0, "rounds": [3], "result": {"winner": "a"}}),
    ];
    let writes = values
        .iter()
        .enumerate()
        .map(|(id, value)| crate::tests::write_to("matches", &id.to_string(), Some(value.clone())))
        .collect();
    store.commit(commit("matches", 2, writes)).unwrap();
    let snapshot = store.snapshot().unwrap();
    for (id, value) in values.iter().enumerate() {
        assert_eq!(
            &snapshot
                .get(&DocumentKey::new("matches", id.to_string()).unwrap())
                .unwrap()
                .unwrap()
                .value,
            value
        );
    }
    let types: (String, String, String, String) = store
        .connection
        .query_row(
            "SELECT typeof(session), typeof(active), typeof(score), typeof(rating) FROM matches WHERE _id = '0'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        types,
        ("text".into(), "integer".into(), "integer".into(), "integer".into())
    );
    let results: (bool, String) = store
        .connection
        .query_row(
            "SELECT (SELECT result IS NULL FROM matches WHERE _id = '0'), (SELECT result FROM matches WHERE _id = '1')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(results, (true, "null".into()));
    let shared_table: usize = store
        .connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'documents'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(shared_table, 0);
}

#[test]
fn additive_schema_changes_preserve_old_snapshots_and_survive_reopen() {
    let (directory, mut store) = open();
    store
        .commit(commit("player", 1, vec![write("a", Some(json!({"coins": 7})))]))
        .unwrap();
    let old = store.snapshot().unwrap();
    let mut expanded = crate::tests::schema();
    let profile = expanded.get_mut("profiles").unwrap();
    profile.fields.insert(
        "rank".into(),
        Field {
            schema: Schema::Integer,
            optional: true,
        },
    );
    profile.indexes.insert("by_rank".into(), vec!["rank".into()]);
    assert_eq!(store.apply_schema(&expanded).unwrap(), Revision(3));
    let migrated = store.snapshot().unwrap();
    let range = IndexRange {
        index: "by_rank".into(),
        prefix: vec![json!(null)],
        ..by_coins()
    };
    assert_eq!(migrated.scan_index(&range).unwrap()[0].0, "a");
    assert!(old.scan_index(&range).is_err());
    store
        .commit(commit(
            "rank",
            3,
            vec![write("a", Some(json!({"coins": 7, "rank": 1})))],
        ))
        .unwrap();
    let key = DocumentKey::new("profiles", "a").unwrap();
    assert_eq!(old.get(&key).unwrap().unwrap().value, json!({"coins": 7}));
    assert_eq!(migrated.get(&key).unwrap().unwrap().value, json!({"coins": 7}));
    assert_eq!(store.apply_schema(&crate::tests::schema()).unwrap(), Revision(4));
    drop(store);
    let mut store = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    assert_eq!(store.apply_schema(&expanded).unwrap(), Revision(4));
    assert_eq!(
        store.snapshot().unwrap().get(&key).unwrap().unwrap().value,
        json!({"coins": 7, "rank": 1})
    );
    assert_eq!(old.get(&key).unwrap().unwrap().revision, Revision(2));
}

#[test]
fn failed_migration_rolls_back_ddl_catalog_and_revision() {
    let (_directory, mut store) = open();
    let mut expanded = crate::tests::schema();
    expanded.get_mut("profiles").unwrap().fields.insert(
        "name".into(),
        Field {
            schema: Schema::String,
            optional: true,
        },
    );
    expanded.insert("matches".into(), TableSchema::default());
    store.connection.execute_batch("CREATE TRIGGER fail_migration BEFORE INSERT ON _chunk_migrations BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
    assert!(store.apply_schema(&expanded).is_err());
    assert_eq!(store.snapshot().unwrap().revision, Revision(1));
    assert_eq!(*store.schema, crate::tests::schema());
    let count: usize = store
        .connection
        .query_row("SELECT count(*) FROM sqlite_schema WHERE name = 'matches'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 0);
    let fields: usize = store
        .connection
        .query_row(
            "SELECT count(*) FROM pragma_table_info('profiles') WHERE name = 'name'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(fields, 0);
    store.connection.execute_batch("DROP TRIGGER fail_migration").unwrap();
    assert_eq!(store.apply_schema(&expanded).unwrap(), Revision(2));
}

#[test]
fn invalid_schemas_and_documents_fail_without_changing_storage() {
    let (_directory, mut store) = open();
    for name in ["_chunk_metadata", "sqlite_sequence", "bad\"name", "Profiles"] {
        let schema = [(name.into(), TableSchema::default())].into();
        assert!(store.apply_schema(&schema).is_err(), "{name}");
    }
    let mut schema = crate::tests::schema();
    schema
        .get_mut("profiles")
        .unwrap()
        .fields
        .get_mut("coins")
        .unwrap()
        .schema = Schema::String;
    assert!(store.apply_schema(&schema).is_err());
    schema = crate::tests::schema();
    schema.get_mut("profiles").unwrap().fields.insert(
        "name".into(),
        Field {
            schema: Schema::String,
            optional: false,
        },
    );
    assert!(store.apply_schema(&schema).is_err());
    schema = crate::tests::schema();
    schema
        .get_mut("profiles")
        .unwrap()
        .indexes
        .insert("bad".into(), vec!["absent".into()]);
    assert!(store.apply_schema(&schema).is_err());
    for value in [
        json!({}),
        json!({"coins": "1"}),
        json!({"coins": null}),
        json!({"coins": 1, "extra": true}),
        json!({"coins": u64::MAX}),
    ] {
        assert!(
            store
                .commit(commit("invalid", 1, vec![write("a", Some(value))]))
                .is_err()
        );
    }
    assert!(
        store
            .commit(commit("duplicate", 1, vec![write("a", None), write("a", None)]))
            .is_err()
    );
    assert!(
        store
            .commit(commit(
                "unknown",
                1,
                vec![crate::tests::write_to("unknown", "a", Some(json!({})))]
            ))
            .is_err()
    );
    assert_eq!(store.snapshot().unwrap().revision, Revision(1));
    assert_eq!(totals(&store), (0, 0));
}

#[test]
fn legacy_format_is_rejected_without_modifying_documents() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("legacy.db");
    let connection = Connection::open(&path).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE documents (value TEXT); INSERT INTO documents VALUES ('preserve'); PRAGMA user_version = 1;",
        )
        .unwrap();
    assert!(matches!(
        SqliteStore::open(&path, "local"),
        Err(Error::SchemaVersion(1))
    ));
    let value: String = connection
        .query_row("SELECT value FROM documents", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, "preserve");
}

#[test]
fn index_can_be_added_to_a_retained_but_omitted_field() {
    let (directory, mut store) = open();
    store
        .commit(commit("seed", 1, vec![write("a", Some(json!({"coins": 7})))]))
        .unwrap();
    let old = store.snapshot().unwrap();
    let partial: DatabaseSchema = serde_json::from_value(json!({
        "profiles": {"fields": {}, "indexes": {"by_retained_coins": ["coins"]}}
    }))
    .unwrap();
    assert_eq!(store.apply_schema(&partial).unwrap(), Revision(3));
    let range = IndexRange {
        index: "by_retained_coins".into(),
        ..by_coins()
    };
    assert_eq!(store.snapshot().unwrap().scan_index(&range).unwrap()[0].0, "a");
    assert!(old.scan_index(&range).is_err());
    assert_eq!(store.apply_schema(&partial).unwrap(), Revision(3));
    drop(store);
    let mut store = SqliteStore::open(directory.path().join("data.db"), "local").unwrap();
    assert_eq!(
        store.snapshot().unwrap().scan_index(&range).unwrap()[0].1.value,
        json!({"coins": 7})
    );
}
