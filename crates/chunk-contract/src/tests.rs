use std::collections::BTreeMap;

use serde_json::json;

use crate::{DatabaseSchema, Field, Schema, TableSchema, validate};

fn field(schema: Schema) -> Field {
    Field { schema, optional: false }
}

#[test]
fn typed_ids_preserve_table_identity_and_platform_namespaces() {
    let id = Schema::Id { table: "profiles".into() };
    assert!(id.accepts(&json!("profiles:p1")));
    for invalid in [json!("matches:p1"), json!("profiles:"), json!("profiles:bad id"), json!(null)] {
        assert!(!id.accepts(&invalid));
    }
    assert!(Schema::Player.accepts(&json!("player-1")));
    assert!(Schema::Session.accepts(&json!("session-1")));
    assert!(!Schema::Player.accepts(&json!("profiles:p1")));
    assert_eq!(serde_json::to_value(&id).unwrap(), json!({"type":"id", "table":"profiles"}));
    assert!(
        validate(&database(TableSchema {
            fields: [("ref".into(), field(id))].into(),
            indexes: [("by_ref".into(), vec!["ref".into()])].into()
        }))
        .is_ok()
    );
}

fn database(table: TableSchema) -> DatabaseSchema {
    [("profiles".into(), table)].into()
}

#[test]
fn nested_documents_require_declared_properties_without_scalar_coercion() {
    let schema = Schema::Object {
        fields: [
            ("items".into(), field(Schema::Array { items: Box::new(Schema::Integer) })),
            ("state".into(), field(Schema::Nullable { value: Box::new(Schema::Literal { value: json!("ready") }) })),
            ("label".into(), Field { schema: Schema::String, optional: true }),
        ]
        .into(),
    };
    for value in
        [json!({"items": [], "state": null}), json!({"items": [i64::MIN, i64::MAX], "state": "ready", "label": ""})]
    {
        assert!(schema.accepts(&value));
    }
    for value in [
        json!({"items": ["1"], "state": null}),
        json!({"items": [1.0], "state": null}),
        json!({"items": [u64::MAX], "state": null}),
        json!({"items": [], "state": "unknown"}),
        json!({"items": []}),
        json!({"items": [], "state": null, "label": null}),
        json!({"items": [], "state": null, "extra": 1}),
    ] {
        assert!(!schema.accepts(&value), "{value}");
    }
    assert!(Schema::Number.accepts(&json!(i64::MAX)));
    assert!(Schema::Number.accepts(&json!(1.5)));
    assert!(!Schema::Number.accepts(&json!(u64::MAX)));
}

#[test]
fn complete_database_validation_rejects_name_collisions_and_invalid_indexes() {
    let table = TableSchema {
        fields: [("coins".into(), field(Schema::Integer))].into(),
        indexes: [("by_coins".into(), vec!["coins".into()])].into(),
    };
    assert!(validate(&database(table.clone())).is_ok());
    let collision = [("profiles".into(), table.clone()), ("Profiles".into(), table.clone())].into();
    assert!(validate(&collision).is_err());
    for name in ["", "_internal", "sqlite_reserved", "bad-name", "1table", "é"] {
        assert!(validate(&[(name.into(), table.clone())].into()).is_err(), "{name}");
    }
    for fields in [vec![], vec!["missing".into()], vec!["coins".into(), "coins".into()]] {
        let mut invalid = table.clone();
        invalid.indexes.insert("bad".into(), fields);
        assert!(validate(&database(invalid)).is_err());
    }
    let mut complex = table.clone();
    complex.fields.get_mut("coins").unwrap().schema = Schema::Nullable { value: Box::new(Schema::Integer) };
    assert!(validate(&database(complex)).is_err());
    let mut collision = table.clone();
    collision.fields.insert("Coins".into(), field(Schema::Integer));
    assert!(validate(&database(collision)).is_err());
    let mut collision = table;
    collision.indexes.insert("By_coins".into(), vec!["coins".into()]);
    assert!(validate(&database(collision)).is_err());
}

#[test]
fn declaration_limits_accept_the_boundary_and_reject_one_more() {
    let mut tables: DatabaseSchema = (0..128).map(|i| (format!("table{i}"), TableSchema::default())).collect();
    assert!(validate(&tables).is_ok());
    tables.insert("overflow".into(), TableSchema::default());
    assert!(validate(&tables).is_err());
    let mut table = TableSchema {
        fields: (0..64).map(|i| (format!("field{i}"), field(Schema::Integer))).collect(),
        indexes: (0..16).map(|i| (format!("index{i}"), (0..8).map(|j| format!("field{j}")).collect())).collect(),
    };
    assert!(validate(&database(table.clone())).is_ok());
    table.fields.insert("overflow".into(), field(Schema::Integer));
    assert!(validate(&database(table.clone())).is_err());
    table.fields.remove("overflow");
    table.indexes.insert("overflow".into(), vec!["field0".into()]);
    assert!(validate(&database(table.clone())).is_err());
    table.indexes.remove("overflow");
    table.indexes.get_mut("index0").unwrap().push("field8".into());
    assert!(validate(&database(table)).is_err());
}

#[test]
fn union_depth_and_literal_limits_are_checked_before_storage() {
    let table =
        |schema| database(TableSchema { fields: [("value".into(), field(schema))].into(), indexes: BTreeMap::new() });
    assert!(
        validate(&table(Schema::Union {
            variants: (0..16).map(|i| (format!("v{i}"), Schema::Object { fields: BTreeMap::new() })).collect()
        }))
        .is_ok()
    );
    assert!(
        validate(&table(Schema::Union {
            variants: (0..17).map(|i| (format!("v{i}"), Schema::Object { fields: BTreeMap::new() })).collect()
        }))
        .is_err()
    );
    assert!(validate(&table(Schema::Union { variants: BTreeMap::new() })).is_err());
    for value in [json!(null), json!(true), json!(1), json!("x")] {
        assert!(validate(&table(Schema::Literal { value })).is_ok());
    }
    for value in [json!([]), json!({})] {
        assert!(validate(&table(Schema::Literal { value })).is_err());
    }
    let mut nested = Schema::Integer;
    for _ in 0..32 {
        nested = Schema::Array { items: Box::new(nested) };
    }
    assert!(validate(&table(nested.clone())).is_ok());
    assert!(validate(&table(Schema::Array { items: Box::new(nested) })).is_err());
}

#[test]
fn tagged_unions_enums_and_api_null_normalization_share_one_schema() {
    let state = Schema::Union {
        variants: [
            ("ready".into(), Schema::Object { fields: BTreeMap::new() }),
            (
                "waiting".into(),
                Schema::Object { fields: [("reason".into(), Field { schema: Schema::String, optional: true })].into() },
            ),
        ]
        .into(),
    };
    assert!(state.accepts(&json!({"type":"ready"})));
    assert!(state.accepts(&json!({"type":"waiting","reason":"busy"})));
    for invalid in [json!("ready"), json!({}), json!({"type":"other"}), json!({"type":"ready","reason":"extra"})] {
        assert!(!state.accepts(&invalid));
    }
    let schema = Schema::Object {
        fields: [
            ("state".into(), field(state)),
            ("note".into(), Field { schema: Schema::String, optional: true }),
            (
                "result".into(),
                field(Schema::Nullable {
                    value: Box::new(Schema::Enum { values: vec!["allow".into(), "deny".into()] }),
                }),
            ),
        ]
        .into(),
    };
    let mut value = json!({"state":{"type":"waiting","reason":null},"note":null,"result":null});
    assert!(!schema.accepts(&value));
    schema.normalize_api(&mut value);
    assert_eq!(value, json!({"state":{"type":"waiting"},"result":null}));
    assert!(schema.accepts(&value));
    value["result"] = json!("unknown");
    assert!(!schema.accepts(&value));
    value["result"] = json!("allow");
    assert!(schema.accepts(&value));
}

#[test]
fn vars_resolve_by_environment_name() {
    let manifest = crate::EnvManifest {
        vars: [("MOTD".into(), "hello".into()), ("REGION".into(), "na".into())].into(),
        environments: [("prod".into(), [("MOTD".into(), "welcome".into())].into())].into(),
        secrets: ["API_KEY".into()].into(),
    };
    assert!(manifest.validate().is_ok());
    let resolved = |environment| manifest.resolve(environment).into_iter().collect::<Vec<_>>();
    let top = vec![("MOTD".to_owned(), "hello".to_owned()), ("REGION".to_owned(), "na".to_owned())];
    assert_eq!(resolved(None), top);
    assert_eq!(resolved(Some("staging")), top);
    assert_eq!(resolved(Some("prod")), [("MOTD".to_owned(), "welcome".to_owned()), top[1].clone()]);
    let shadowed = crate::EnvManifest { secrets: ["REGION".into()].into(), ..manifest };
    assert!(shadowed.validate().is_err());
    let section = |names: std::ops::Range<usize>| names.map(|name| (format!("V{name}"), "value".to_owned())).collect();
    let spread = crate::EnvManifest {
        environments: [("a".into(), section(0..200)), ("b".into(), section(200..257))].into(),
        ..crate::EnvManifest::default()
    };
    assert!(spread.validate().unwrap_err().contains("more than 256 variables in all"));
}

#[test]
fn migration_snapshots_must_follow_their_declared_changes() {
    use crate::{Migration, MigrationKind, MigrationTable, validate_migrations};
    let schema = |fields: &[(&str, Schema)]| -> DatabaseSchema {
        let fields = fields.iter().map(|(name, schema)| ((*name).to_owned(), field(schema.clone()))).collect();
        [("fighters".to_owned(), TableSchema { fields, indexes: BTreeMap::new() })].into()
    };
    let table = |added: &[&str], removed: &[&str], back| MigrationTable {
        added: added.iter().map(ToString::to_string).collect(),
        removed: removed.iter().map(ToString::to_string).collect(),
        back,
    };
    let entry = |id: &str, kind, finishes: Option<&str>, tables: &[(&str, MigrationTable)], schema| Migration {
        id: id.into(),
        hash: "0".repeat(64),
        kind,
        finishes: finishes.map(Into::into),
        tables: tables.iter().map(|(name, table)| ((*name).to_owned(), table.clone())).collect(),
        schema,
    };
    let old = schema(&[("name", Schema::String)]);
    let new = schema(&[("displayName", Schema::String)]);
    let baseline = entry("0001_init", MigrationKind::Baseline, None, &[], old.clone());
    let expand =
        |schema, tables: &[(&str, MigrationTable)]| entry("0002_a", MigrationKind::Expand, None, tables, schema);
    let finish = |schema, tables: &[(&str, MigrationTable)]| {
        entry("0003_finish_a", MigrationKind::Finish, Some("0002_a"), tables, schema)
    };
    let rename = [("fighters", table(&["displayName"], &["name"], true))];
    let good = [
        baseline.clone(),
        expand(new.clone(), &rename),
        finish(new.clone(), &[("fighters", table(&[], &["name"], false))]),
    ];
    validate_migrations(&good).unwrap();

    let undeclared = schema(&[("name", Schema::Number)]);
    let wrong_table = [("players", table(&["displayName"], &["name"], true))];
    let rejected = [
        vec![baseline.clone(), expand(undeclared, &rename)],
        vec![baseline.clone(), expand(new.clone(), &[("fighters", table(&["displayName"], &[], true))])],
        vec![baseline.clone(), expand(new.clone(), &wrong_table)],
        vec![
            baseline.clone(),
            expand(new.clone(), &rename),
            finish(old.clone(), &[("fighters", table(&[], &["name"], false))]),
        ],
        vec![
            baseline.clone(),
            expand(new.clone(), &rename),
            finish(new.clone(), &[("fighters", table(&[], &["rank"], false))]),
        ],
        vec![
            baseline.clone(),
            expand(new.clone(), &rename),
            finish(new.clone(), &[("fighters", table(&["x"], &["name"], false))]),
        ],
        vec![baseline, expand(new.clone(), &rename), finish(new, &[("fighters", table(&[], &["name"], true))])],
    ];
    for migrations in rejected {
        assert!(validate_migrations(&migrations).is_err(), "{migrations:?}");
    }
}
