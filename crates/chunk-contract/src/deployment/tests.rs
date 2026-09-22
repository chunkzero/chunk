use super::*;
use serde_json::json;

fn deployment() -> Deployment {
    Deployment {
        contracts: Contracts::default(),
        contract_version: CONTRACT_VERSION,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: "v1".into(),
        source: "export function read() { return null }".into(),
        tables: BTreeMap::new(),
        functions: [(
            "shared/profile".into(),
            Function {
                kind: FunctionKind::Query,
                visibility: Visibility::Public,
                export: "read".into(),
                arguments: Schema::Object { fields: BTreeMap::new() },
                result: Schema::Null,
            },
        )]
        .into(),
    }
}

#[test]
fn versioned_contract_round_trip_and_namespace_validation() {
    let original = deployment();
    let copy: Deployment = serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
    assert_eq!(original, copy);
    copy.validate().unwrap();
    for path in ["shared", "Shared/Profile", "shared//other", "../escape"] {
        let mut invalid = original.clone();
        let mut function = invalid.functions.values().next().unwrap().clone();
        function.export = "other".into();
        invalid.functions.insert(path.into(), function);
        assert!(invalid.validate().is_err(), "{path}");
    }
    let mut invalid = original;
    invalid.contract_version += 1;
    assert!(invalid.validate().is_err());
    let mut encoded = serde_json::to_value(deployment()).unwrap();
    encoded["runtime_profile"] = json!("unknown");
    assert!(serde_json::from_value::<Deployment>(encoded).is_err());
    let mut encoded = serde_json::to_value(deployment()).unwrap();
    encoded["unknown"] = json!(1);
    assert!(serde_json::from_value::<Deployment>(encoded).is_err());
}

#[test]
fn wire_values_preserve_optional_null_and_integer_boundaries() {
    for value in [
        json!(9_007_199_254_740_991_i64),
        json!(-9_007_199_254_740_991_i64),
        json!(9_007_199_254_740_991_f64),
        json!(0.125),
        json!("100000000000000000000"),
    ] {
        validate_wire_value(&value).unwrap();
    }
    for value in [
        json!(9_007_199_254_740_992_i64),
        json!(9_007_199_254_740_992_f64),
        json!(1e20),
        json!(-1e20),
        json!(i64::MIN),
        json!({"nested": [u64::MAX]}),
    ] {
        assert!(validate_wire_value(&value).is_err());
    }
    let schema =
        Schema::Object { fields: [("value".into(), crate::Field { schema: Schema::String, optional: true })].into() };
    assert!(schema.accepts(&json!({})));
    assert!(schema.accepts(&json!({"value": "text"})));
    assert!(!schema.accepts(&json!({"value": null})));
    let mut invalid = deployment();
    invalid.functions.values_mut().next().unwrap().result = Schema::Union { variants: BTreeMap::new() };
    assert!(invalid.validate().is_err());
}

#[test]
fn nested_literal_schemas_share_wire_limits_in_tables_arguments_and_results() {
    for (value, valid) in [
        (json!(9_007_199_254_740_991_i64), true),
        (json!(-9_007_199_254_740_991_f64), true),
        (json!(0.125), true),
        (json!("100000000000000000000"), true),
        (json!(9_007_199_254_740_992_i64), false),
        (json!(9_007_199_254_740_992_f64), false),
        (json!(1e20), false),
        (json!(-1e20), false),
    ] {
        let schema = Schema::Nullable {
            value: Box::new(Schema::Array {
                items: Box::new(Schema::Union {
                    variants: [(
                        "exact".into(),
                        Schema::Object {
                            fields: [(
                                "value".into(),
                                crate::Field { schema: Schema::Literal { value: value.clone() }, optional: true },
                            )]
                            .into(),
                        },
                    )]
                    .into(),
                }),
            }),
        };
        let mut table = deployment();
        table.tables.insert(
            "values".into(),
            crate::TableSchema {
                fields: [("nested".into(), crate::Field { schema: schema.clone(), optional: false })].into(),
                ..Default::default()
            },
        );
        let mut arguments = deployment();
        arguments.functions.values_mut().next().unwrap().arguments = schema.clone();
        let mut result = deployment();
        result.functions.values_mut().next().unwrap().result = schema;
        for (location, contract) in [("table", table), ("arguments", arguments), ("result", result)] {
            assert_eq!(contract.validate().is_ok(), valid, "{location}: {value}");
        }
    }
}
