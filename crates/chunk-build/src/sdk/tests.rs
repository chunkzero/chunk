use super::*;
use std::time::{Duration, SystemTime};

fn project() -> tempfile::TempDir {
    let project = tempfile::tempdir().unwrap();
    fs::create_dir_all(project.path().join("server/schema")).unwrap();
    fs::write(project.path().join("server/schema/index.ts"), "unfinished schema").unwrap();
    project
}

#[test]
fn generation_repairs_missing_and_stale_files_without_rewriting_unchanged_files() {
    let project = project();
    generate_sdk(project.path()).unwrap();
    let mut files: Vec<_> = SOURCES.iter().map(|(name, _)| project.path().join(".chunk/sdk").join(name)).collect();
    files.extend([".chunk/generated/index.ts", "package.json", "tsconfig.json"].map(|name| project.path().join(name)));
    let timestamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let contents: Vec<_> = files
        .iter()
        .map(|file| {
            fs::File::options().write(true).open(file).unwrap().set_modified(timestamp).unwrap();
            fs::read(file).unwrap()
        })
        .collect();
    generate_sdk(project.path()).unwrap();
    for (file, content) in files.iter().zip(&contents) {
        assert_eq!(fs::read(file).unwrap(), *content);
        assert_eq!(fs::metadata(file).unwrap().modified().unwrap(), timestamp);
    }
    fs::remove_file(&files[0]).unwrap();
    fs::write(&files[1], "stale SDK").unwrap();
    fs::write(project.path().join(".chunk/generated/index.ts"), "stale bindings").unwrap();
    generate_sdk(project.path()).unwrap();
    for (file, content) in files.iter().zip(&contents) {
        assert_eq!(fs::read(file).unwrap(), *content);
    }
    assert!(!project.path().join(".chunk/build").exists());
}

#[test]
fn generation_preserves_project_configuration_and_repairs_owned_imports() {
    let project = project();
    let package_path = project.path().join("package.json");
    let package = json!({
        "name": "example", "type": "commonjs",
        "scripts": { "test": "custom-test" }, "dependencies": { "other": "1.0.0" },
        "imports": { "#other": { "import": "./esm.ts", "default": "./fallback.ts" }, "#chunk": "./old.ts" },
        "exports": { "import": "./esm.ts", "default": "./fallback.ts" }
    });
    fs::write(&package_path, serde_json::to_vec(&package).unwrap()).unwrap();
    let config = "{\n  // Keep project-specific options and formatting.\n  \"extends\": \"../tsconfig.json\"\n}\n";
    fs::write(project.path().join("tsconfig.json"), config).unwrap();
    generate_sdk(project.path()).unwrap();
    let mut generated: Value = serde_json::from_slice(&fs::read(package_path).unwrap()).unwrap();
    assert_eq!(generated["imports"]["#chunk"], "./.chunk/generated/index.ts");
    assert_eq!(generated["imports"]["#chunk/schema"], "./.chunk/sdk/schema.ts");
    generated["imports"].as_object_mut().unwrap().remove("#chunk/schema");
    generated["imports"].as_object_mut().unwrap().remove("#chunk/apps");
    generated["imports"]["#chunk"] = json!("./old.ts");
    assert_eq!(serde_json::to_string(&generated).unwrap(), serde_json::to_string(&package).unwrap());
    assert_eq!(fs::read_to_string(project.path().join("tsconfig.json")).unwrap(), config);
}

#[test]
fn generation_reports_missing_schema_and_invalid_configuration_without_overwriting_it() {
    let empty = tempfile::tempdir().unwrap();
    assert!(generate_sdk(empty.path()).unwrap_err().to_string().contains("server/schema/index.ts"));
    assert!(!empty.path().join(".chunk").exists());
    let project = project();
    for invalid in ["{", "[]", "{\"imports\":null}"] {
        let package = project.path().join("package.json");
        fs::write(&package, invalid).unwrap();
        assert!(generate_sdk(project.path()).unwrap_err().to_string().contains("package.json"));
        assert_eq!(fs::read_to_string(package).unwrap(), invalid);
        assert!(!project.path().join(".chunk").exists());
    }
}

#[test]
fn generated_implementation_refs_require_an_authored_catalog() {
    let project = project();
    let root = project.path();
    for (id, manifest, source) in [
        ("overrides", "app.toml", "[sessions.large]\ncapacity = 32"),
        ("unspecified", "app.toml", ""),
        ("authored", "app.ts", "export default defineApp({id:'authored'});"),
    ] {
        let directory = root.join("apps").join(id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join(manifest), source).unwrap();
        fs::write(directory.join("build.gradle.kts"), "").unwrap();
    }
    generate_sdk(root).unwrap();
    let source = fs::read_to_string(root.join(".chunk/generated/apps.ts")).unwrap();
    for legacy in ["overrides", "unspecified"] {
        assert!(
            source.contains(&format!("[\"{legacy}\"]:{{[\"id\"]:\"{legacy}\",\n[\"implementations\"]:{{}}")),
            "{source}"
        );
    }
    assert!(!source.contains("[\"large\"]"), "{source}");
    assert!(source.contains("[\"default\"]:{[\"app\"]:\"authored\",\n[\"session\"]:\"default\"}"), "{source}");
}
