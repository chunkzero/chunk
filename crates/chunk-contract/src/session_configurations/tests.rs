use super::*;
use serde_json::json;

fn catalog() -> SessionConfigurations {
    serde_json::from_value(json!({"version":1,"configurations":[{
        "app":"arena","session":"default","configuration":{"type":"object","fields":{
            "map":{"schema":{"type":"enum","values":["forest","desert"]}},
            "rounds":{"schema":{"type":"integer"}}
        }}
    }]}))
    .unwrap()
}

#[test]
fn creation_configuration_checks_exact_schema_and_wire_limits() {
    let catalog = catalog();
    catalog.validate_configuration("arena/default", &json!({"map":"forest","rounds":3})).unwrap();
    for value in [
        json!({}),
        json!({"map":"ocean","rounds":3}),
        json!({"map":"forest","rounds":"3"}),
        json!({"map":"forest","rounds":3,"extra":true}),
        json!({"map":"forest","rounds":9_007_199_254_740_992_i64}),
        json!({"map":"x".repeat(MAX_SESSION_CONFIGURATION_BYTES),"rounds":3}),
        json!([]),
    ] {
        assert!(catalog.validate_configuration("arena/default", &value).is_err(), "{value}");
    }
    validate_session_configuration(None, "arena/default", &json!({})).unwrap();
    assert!(validate_session_configuration(None, "arena/default", &json!({"map":"forest"})).is_err());
    assert!(catalog.validate_configuration("arena/other", &json!({"map":"forest","rounds":3})).is_err());
}

#[test]
fn configuration_catalog_rejects_unknown_versions_and_implementations() {
    let catalog = catalog();
    let app: AppArtifact = serde_json::from_value(json!({
        "id":"arena","jar":"arena.jar","sha256":"artifact","java_version":25,
        "sessions":{"default":{"machine_profile":"small","capacity":16}}
    }))
    .unwrap();
    catalog.validate_apps(&[("arena".into(), app)].into()).unwrap();
    assert!(catalog.validate_apps(&BTreeMap::new()).is_err());
    for change in ["version", "duplicate", "schema"] {
        let mut invalid = serde_json::to_value(&catalog).unwrap();
        match change {
            "version" => invalid["version"] = json!(2),
            "duplicate" => {
                let mut duplicate = invalid["configurations"][0].clone();
                duplicate["app"] = json!("Arena");
                invalid["configurations"].as_array_mut().unwrap().push(duplicate);
            }
            _ => invalid["configurations"][0]["configuration"] = json!({"type":"string"}),
        }
        assert!(serde_json::from_value::<SessionConfigurations>(invalid).is_err(), "{change}");
    }
}
