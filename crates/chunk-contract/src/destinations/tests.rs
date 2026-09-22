use super::*;
use serde_json::json;

#[test]
fn identities_and_catalog_bindings_are_immutable_and_versioned() {
    let manifest: DestinationManifest=serde_json::from_value(json!({"version":1,"entries":{
        "shared/destinations/main":{"destination":{"key":"main","session_type":"lobby/default","machine_profile":"local"},"overflow":"reject","empty_timeout_seconds":60}
    }})).unwrap();
    manifest.validate().unwrap();
    let mut duplicate = manifest.clone();
    duplicate.entries.insert("shared/destinations/other".into(), manifest.entries.values().next().unwrap().clone());
    assert!(duplicate.validate().is_err());
    let mut changed = manifest.clone();
    changed.version = 2;
    assert!(changed.validate().is_err());
    let app:AppArtifact=serde_json::from_value(json!({"id":"lobby","jar":"lobby.jar","sha256":"artifact","java_version":25,"sessions":{"default":{"machine_profile":"local","capacity":16}}})).unwrap();
    let apps = [("lobby".into(), app)].into();
    manifest.validate_apps(&apps).unwrap();
    let mut changed = manifest.clone();
    changed.entries.values_mut().next().unwrap().destination.machine_profile = "other".into();
    assert!(changed.validate_apps(&apps).is_err());
    let mut unsupported = serde_json::to_value(manifest).unwrap();
    unsupported["entries"]["shared/destinations/main"]["parameters"] = json!({"map":"other"});
    assert!(serde_json::from_value::<DestinationManifest>(unsupported).is_err());
}
