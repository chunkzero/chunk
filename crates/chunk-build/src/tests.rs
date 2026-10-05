use super::*;
use std::fs;

#[test]
fn codegen_rejects_colliding_names_and_unsupported_literals_before_writing() {
    use serde_json::json;

    let root = tempfile::tempdir().unwrap();
    let contract_file = root.path().join("contract.json");
    let output = root.path().join("output");
    let original: serde_json::Value =
        serde_json::from_str(include_str!("../../../jvm/backend-api/src/test/resources/contract.json")).unwrap();
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
            GenerationTarget::Java { package: "com.chunkzero.chunk.generated" },
        )
        .unwrap_err();
        assert!(error.to_string().contains(message), "{error}");
        assert!(!output.exists());
    }
}
