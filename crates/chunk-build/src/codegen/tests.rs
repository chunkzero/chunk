use std::{fs, path::Path};

use super::{GenerationTarget, generate};

fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../jvm/backend-api/src/test/resources/contract.json")
}

#[test]
fn java_rejects_lexical_collisions_without_writing_partial_sources() {
    use serde_json::json;

    let original: serde_json::Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    let mut namespace = original.clone();
    namespace["functions"]["shared/player_stats/read"] = json!({
        "kind": "query", "visibility": "public", "export": "snakeCase",
        "arguments": {"type": "null"}, "result": {"type": "null"}
    });
    namespace["functions"]["shared/playerStats/write"] = json!({
        "kind": "query", "visibility": "public", "export": "camelCase",
        "arguments": {"type": "null"}, "result": {"type": "null"}
    });
    let mut fields = original.clone();
    fields["functions"]["shared/profile/total"]["arguments"] = json!({
        "type": "object", "fields": {
            "class": {"schema": {"type": "string"}},
            "class_": {"schema": {"type": "string"}}
        }
    });
    let mut watch = original;
    watch["functions"]["shared/profile/watch_total"] = json!({
        "kind": "query", "visibility": "public", "export": "watchName",
        "arguments": {"type": "null"}, "result": {"type": "null"}
    });
    for (contract, symbol) in [(namespace, "PlayerStats"), (fields, "class_"), (watch, "watchTotal")] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("contract.json");
        fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
        let output = root.path().join("output");
        let error = generate(&path, &output, GenerationTarget::Java { package: "example" }).unwrap_err();
        assert!(error.to_string().contains(&format!("{symbol} collides")), "{error}");
        assert!(!output.exists());
    }
}

#[test]
fn selected_outputs_replace_stale_packages_and_preserve_handwritten_files() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("generated");
    let first = GenerationTarget::Java { package: "example.first" };
    generate(&fixture(), &output, first).unwrap();
    let types = output.join("java/example/first/BackendTypes.java");
    let client = output.join("java-client/example/first/BackendClient.java");
    let original = fs::read(&types).unwrap();
    assert!(client.is_file());
    assert!(!output.join("api.ts").exists());
    generate(&fixture(), &output, first).unwrap();
    assert_eq!(original, fs::read(&types).unwrap());
    let handwritten = output.join("java/example/first/Custom.java");
    fs::write(&handwritten, "// Handwritten").unwrap();
    generate(&fixture(), &output, GenerationTarget::Java { package: "example.second" }).unwrap();
    assert!(!types.exists());
    assert!(!client.exists());
    assert!(output.join("java/example/second/BackendTypes.java").is_file());
    generate(&fixture(), &output, GenerationTarget::TypeScript).unwrap();
    assert!(output.join("api.ts").is_file());
    assert!(!output.join("java/example/second/BackendTypes.java").exists());
    assert!(!output.join("java-client/example/second/BackendClient.java").exists());
    assert_eq!(fs::read_to_string(handwritten).unwrap(), "// Handwritten");
}

#[test]
fn output_conflicts_do_not_replace_handwritten_or_modified_files() {
    let root = tempfile::tempdir().unwrap();
    let api = root.path().join("api.ts");
    fs::write(&api, "// Handwritten").unwrap();
    let error = generate(&fixture(), root.path(), GenerationTarget::TypeScript).unwrap_err();
    assert!(error.to_string().contains("unowned file"), "{error}");
    assert_eq!(fs::read_to_string(&api).unwrap(), "// Handwritten");
    fs::remove_file(&api).unwrap();
    generate(&fixture(), root.path(), GenerationTarget::TypeScript).unwrap();
    fs::write(&api, "// Edited generated source").unwrap();
    let error = generate(&fixture(), root.path(), GenerationTarget::Java { package: "example.backend" }).unwrap_err();
    assert!(error.to_string().contains("generated file was modified"), "{error}");
    assert_eq!(fs::read_to_string(api).unwrap(), "// Edited generated source");
    assert!(!root.path().join("java").exists());
}

#[test]
fn ownership_records_cannot_remove_files_outside_the_destination() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("generated");
    fs::create_dir(&output).unwrap();
    let outside = root.path().join("outside.ts");
    fs::write(&outside, "// Handwritten").unwrap();
    fs::write(output.join(".chunk-codegen.json"), r#"{"version":1,"files":{"../outside.ts":"invalid"}}"#).unwrap();
    let error = generate(&fixture(), &output, GenerationTarget::TypeScript).unwrap_err();
    assert!(error.to_string().contains("invalid generated relative path"), "{error}");
    assert_eq!(fs::read_to_string(outside).unwrap(), "// Handwritten");
    assert!(!output.join("api.ts").exists());
}

#[cfg(unix)]
#[test]
fn owned_source_directories_cannot_be_redirected_through_symlinks() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("generated");
    let target = GenerationTarget::Java { package: "example.backend" };
    generate(&fixture(), &output, target).unwrap();
    let outside = root.path().join("outside");
    fs::rename(output.join("java"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, output.join("java")).unwrap();
    let error = generate(&fixture(), &output, GenerationTarget::TypeScript).unwrap_err();
    assert!(error.to_string().contains("cannot traverse symlinks"), "{error}");
    assert!(outside.join("example/backend/BackendTypes.java").is_file());
    assert!(!output.join("api.ts").exists());
}

#[test]
fn kotlin_target_reuses_java_sources_and_removes_its_facade_when_disabled() {
    let root = tempfile::tempdir().unwrap();
    let target = GenerationTarget::Java { package: "example.backend" };
    generate(&fixture(), root.path(), target).unwrap();
    let java = root.path().join("java/example/backend/BackendTypes.java");
    let java_client = root.path().join("java-client/example/backend/BackendClient.java");
    let original = fs::read(&java).unwrap();
    let original_client = fs::read(&java_client).unwrap();
    generate(&fixture(), root.path(), GenerationTarget::Kotlin { package: "example.backend" }).unwrap();
    assert_eq!(original, fs::read(java).unwrap());
    assert_eq!(original_client, fs::read(java_client).unwrap());
    let kotlin = root.path().join("kotlin/example/backend/CoroutineBackendClient.kt");
    assert!(kotlin.is_file());
    assert!(!root.path().join("api.ts").exists());
    generate(&fixture(), root.path(), target).unwrap();
    assert!(!kotlin.exists());
}

#[test]
fn action_references_are_generated_for_typescript_without_becoming_jvm_mutations() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let mut contract: serde_json::Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    contract["functions"]["shared/tasks/work"] = json!({
        "kind":"action", "visibility":"public", "export":"actionWork",
        "arguments":{"type":"null"}, "result":{"type":"integer"}
    });
    contract["functions"]["shared/tasks/read"] = json!({
        "kind":"query", "visibility":"internal", "export":"actionRead",
        "arguments":{"type":"null"}, "result":{"type":"integer"}
    });
    let path = root.path().join("contract.json");
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    let output = root.path().join("generated");
    generate(&path, &output, GenerationTarget::TypeScript).unwrap();
    let source = fs::read_to_string(output.join("api.ts")).unwrap();
    assert!(source.contains("kind: \"action\""));
    assert!(source.contains("export const internal ="));
    assert!(source.contains("shared/tasks/read"));
    generate(&path, &output, GenerationTarget::Kotlin { package: "example" }).unwrap();
    for file in [
        "java/example/BackendTypes.java",
        "java-client/example/BackendClient.java",
        "kotlin/example/CoroutineBackendClient.kt",
    ] {
        let source = fs::read_to_string(output.join(file)).unwrap();
        assert!(!source.contains("shared/tasks/work"));
        assert!(!source.contains("shared/tasks/read"));
    }
}

#[test]
fn session_methods_reject_invalid_versions_identities_and_java_collisions() {
    use serde_json::json;
    let original: serde_json::Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    let method = json!({"app":"duels","session":"default","name":"forfeit","arguments":{"type":"object","fields":{}},"result":{"type":"boolean"}});
    let mut contract = original;
    contract["session_methods"] = json!({"version":1,"methods":[method]});
    let mut version = contract.clone();
    version["session_methods"]["version"] = json!(2);
    let mut duplicate = contract.clone();
    duplicate["session_methods"]["methods"].as_array_mut().unwrap().push(method.clone());
    let mut invalid_args = contract.clone();
    invalid_args["session_methods"]["methods"][0]["arguments"] = json!({"type":"string"});
    let mut collision = contract;
    let mut other = method;
    other["name"] = json!("for_feit");
    collision["session_methods"]["methods"][0]["name"] = json!("forFeit");
    collision["session_methods"]["methods"].as_array_mut().unwrap().push(other);
    for (contract, expected) in [
        (version, "unsupported session method"),
        (duplicate, "duplicate session method"),
        (invalid_args, "arguments must be an object"),
        (collision, "collides"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("contract.json");
        fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
        let error =
            generate(&path, &root.path().join("output"), GenerationTarget::Java { package: "example" }).unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
    }
}

#[test]
fn session_configuration_providers_are_scoped_to_their_app_and_detect_java_collisions() {
    use serde_json::json;
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("generated");
    generate(&fixture(), &output, GenerationTarget::Java { package: "example" }).unwrap();
    let provider = fs::read_to_string(output.join("java-session/duels/example/DuelsSessionProviders.java")).unwrap();
    assert!(provider.contains("ConfiguredSessionProvider<SessionConfigs.Duels.Default.Config>"));
    assert!(provider.contains("return SessionConfigs.Duels.Default.TYPE;"));
    let models = fs::read_to_string(output.join("java/example/SessionConfigs.java")).unwrap();
    assert!(!models.contains("dev.chunkzero.runtime"));
    let mut contract: serde_json::Value = serde_json::from_slice(&fs::read(fixture()).unwrap()).unwrap();
    let mut other = contract["session_configurations"]["configurations"][0].clone();
    other["session"] = json!("some_name");
    contract["session_configurations"]["configurations"][0]["session"] = json!("someName");
    contract["session_configurations"]["configurations"].as_array_mut().unwrap().push(other);
    let path = root.path().join("contract.json");
    fs::write(&path, serde_json::to_vec(&contract).unwrap()).unwrap();
    assert!(
        generate(&path, &output, GenerationTarget::Java { package: "example" })
            .unwrap_err()
            .to_string()
            .contains("collides")
    );
}
