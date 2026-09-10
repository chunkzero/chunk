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
    let first = GenerationTarget::Java {
        package: "example.first",
    };
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
    generate(
        &fixture(),
        &output,
        GenerationTarget::Java {
            package: "example.second",
        },
    )
    .unwrap();
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
    let error = generate(
        &fixture(),
        root.path(),
        GenerationTarget::Java {
            package: "example.backend",
        },
    )
    .unwrap_err();
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
    fs::write(
        output.join(".chunk-codegen.json"),
        r#"{"version":1,"files":{"../outside.ts":"invalid"}}"#,
    )
    .unwrap();
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
    let target = GenerationTarget::Java {
        package: "example.backend",
    };
    generate(&fixture(), &output, target).unwrap();
    let outside = root.path().join("outside");
    fs::rename(output.join("java"), &outside).unwrap();
    std::os::unix::fs::symlink(&outside, output.join("java")).unwrap();
    let error = generate(&fixture(), &output, GenerationTarget::TypeScript).unwrap_err();
    assert!(error.to_string().contains("cannot traverse symlinks"), "{error}");
    assert!(outside.join("example/backend/BackendTypes.java").is_file());
    assert!(!output.join("api.ts").exists());
}
