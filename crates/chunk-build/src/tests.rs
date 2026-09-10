use super::*;
use std::fs;

#[test]
fn publication_is_reproducible_and_new_inputs_cannot_change_a_running_artifact() {
    let root = tempfile::tempdir().unwrap();
    let distribution = root.path().join("distribution");
    fs::create_dir_all(distribution.join("lib")).unwrap();
    fs::write(distribution.join("lib/gameplay.jar"), b"gameplay-one").unwrap();
    let source = root.path().join("backend.mjs");
    fs::write(&source, "export function status() { return 1; }").unwrap();
    let contract = root.path().join("contract.json");
    fs::write(&contract, br#"{"contract_version":1,"runtime_profile":"transactional_v1","tables":{},"functions":{"status":{"kind":"query","visibility":"public","export":"status","arguments":{"type":"null"},"result":{"type":"integer"}}}}"#).unwrap();
    let inputs = Inputs {
        source,
        contract,
        distribution,
    };
    let artifacts = root.path().join("artifacts");
    let first = publish(&inputs, &artifacts, b"project").unwrap();
    let repeated = publish(&inputs, &artifacts, b"project").unwrap();
    assert_eq!(first.id, repeated.id);
    let bundle: Deployment = serde_json::from_slice(&fs::read(first.directory.join("backend.json")).unwrap()).unwrap();
    assert_eq!(bundle.id, first.id);
    fs::write(inputs.distribution.join("lib/gameplay.jar"), b"gameplay-two").unwrap();
    let second = publish(&inputs, &artifacts, b"project").unwrap();
    assert_ne!(first.id, second.id);
    assert_eq!(
        fs::read(first.directory.join("gameplay/lib/gameplay.jar")).unwrap(),
        b"gameplay-one"
    );
    assert_ne!(second.id, publish(&inputs, &artifacts, b"changed-project").unwrap().id);
    // Extra classpath entries invalidate a published artifact, even when its expected files remain intact.
    fs::write(second.directory.join("gameplay/lib/extra.jar"), b"unexpected").unwrap();
    assert!(publish(&inputs, &artifacts, b"project").is_err());
}

#[cfg(unix)]
#[test]
fn child_executable_remains_runnable_after_the_original_is_replaced() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let original = root.path().join("program");
    fs::write(&original, "#!/bin/sh\nexit 7\n").unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o755)).unwrap();
    let pinned = pin_program(&original, &root.path().join("platform")).unwrap();
    fs::rename(&original, root.path().join("old-program")).unwrap();
    fs::write(&original, "#!/bin/sh\nexit 8\n").unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(std::process::Command::new(&pinned).status().unwrap().code(), Some(7));
    let next = pin_program(&original, &root.path().join("platform")).unwrap();
    assert_ne!(pinned, next);
    assert_eq!(std::process::Command::new(next).status().unwrap().code(), Some(8));
}

#[test]
fn generated_typescript_references_validate_the_cross_language_fixtures() {
    let output = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let fixtures = root.join("jvm/backend-api/src/test/resources");
    super::generate(
        &fixtures.join("contract.json"),
        output.path(),
        GenerationTarget::TypeScript,
    )
    .unwrap();
    let api = fs::read_to_string(output.path().join("api.ts")).unwrap();
    super::generate(
        &fixtures.join("contract.json"),
        output.path(),
        GenerationTarget::TypeScript,
    )
    .unwrap();
    assert_eq!(api, fs::read_to_string(output.path().join("api.ts")).unwrap());
    assert!(!api.contains("hidden"));
    let sdk = root.join("packages/server/src/index.ts");
    fs::write(
        output.path().join("api.ts"),
        api.replace(
            "'@chunk/server'",
            &serde_json::to_string(sdk.to_str().unwrap()).unwrap(),
        ),
    )
    .unwrap();
    let fixtures_json = fs::read_to_string(fixtures.join("values.json")).unwrap();
    let script = format!(
        "import assert from 'node:assert/strict'; import {{api}} from './api.ts'; const fixtures={fixtures_json}; for(const value of fixtures) assert.deepEqual(api.shared.profile.record.arguments.parse(value),value); assert.throws(()=>api.shared.profile.record.arguments.parse({{...fixtures[0],count:9007199254740992}})); assert(Object.hasOwn(api, '__proto__')); assert.equal(Object.getPrototypeOf(api), Object.prototype); assert.equal(api.__proto__.read.path, '__proto__/read');"
    );
    fs::write(output.path().join("check.mjs"), script).unwrap();
    assert!(
        std::process::Command::new("node")
            .arg(output.path().join("check.mjs"))
            .status()
            .unwrap()
            .success()
    );
    fs::write(output.path().join("types.ts"), "import {api} from './api.ts'; const doc = api.__proto__.read.result.parse({}); doc.wins = 2;\n// @ts-expect-error Document IDs are immutable.\ndoc._id = doc._id;\n").unwrap();
    fs::write(output.path().join("tsconfig.json"), serde_json::to_vec(&serde_json::json!({"compilerOptions":{"strict":true,"noEmit":true,"target":"ES2023","module":"ESNext","moduleResolution":"Bundler","allowImportingTsExtensions":true,"exactOptionalPropertyTypes":true,"lib":["ES2023"],"types":[]},"files":["api.ts","types.ts"]})).unwrap()).unwrap();
    assert!(
        std::process::Command::new("node")
            .arg(root.join("node_modules/typescript/bin/tsc"))
            .arg("--project")
            .arg(output.path().join("tsconfig.json"))
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn codegen_rejects_colliding_names_and_unsupported_literals_before_writing() {
    use serde_json::json;

    let root = tempfile::tempdir().unwrap();
    let contract_file = root.path().join("contract.json");
    let output = root.path().join("output");
    let original: serde_json::Value = serde_json::from_str(include_str!(
        "../../../jvm/backend-api/src/test/resources/contract.json"
    ))
    .unwrap();
    let mut collision = original.clone();
    collision["functions"]["shared/profile/record_args/read"] = json!({
        "kind": "query", "visibility": "public", "export": "collision",
        "arguments": {"type": "null"}, "result": {"type": "null"}
    });
    let mut unsafe_literal = original.clone();
    unsafe_literal["tables"]["profiles"]["fields"]["wins"]["schema"] =
        json!({"type": "literal", "value": 9_007_199_254_740_992_i64});
    let mut large_literal = original.clone();
    large_literal["functions"]["Codecs"]["result"] = json!({"type": "literal", "value": "\0".repeat(32_768)});
    let mut invalid_name = original;
    invalid_name["tables"]["profiles"]["fields"]["not-valid"] = json!({"schema": {"type": "null"}, "optional": false});
    for (contract, message) in [
        (collision, "RecordArgs collides"),
        (unsafe_literal, "safe range"),
        (large_literal, "Java string constant limit"),
        (invalid_name, "invalid schema identifier"),
    ] {
        fs::write(&contract_file, serde_json::to_vec(&contract).unwrap()).unwrap();
        let error = super::generate(
            &contract_file,
            &output,
            GenerationTarget::Java {
                package: "dev.chunkzero.generated",
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert!(!output.exists());
    }
}
