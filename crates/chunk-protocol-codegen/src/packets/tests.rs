use super::*;

#[test]
fn registry_generation_uses_dataset_packet_ids() {
    let mut data: Value =
        serde_json::from_str(include_str!("../../../chunk-protocol/data/26.2/protocol.json")).unwrap();
    let snapshot = include_bytes!("../../../chunk-protocol/data/26.2/loginPacket.json");
    for (old, new, name) in [("0x07", "0x107", "registry_data"), ("0x0d", "0x10d", "tags")] {
        let original = crate::registries::generate(&data, snapshot).unwrap().to_string();
        let mappings =
            data["configuration"]["toClient"]["types"]["packet"][1][0]["type"][1]["mappings"].as_object_mut().unwrap();
        let value = mappings.remove(old).unwrap();
        assert_eq!(value, name);
        assert!(crate::registries::generate(&data, snapshot).is_err());
        data["configuration"]["toClient"]["types"]["packet"][1][0]["type"][1]["mappings"][new] = value;
        assert_eq!(
            packet_id(&data, "configuration", "toClient", name).unwrap(),
            i32::from_str_radix(&new[2..], 16).unwrap()
        );
        assert_ne!(crate::registries::generate(&data, snapshot).unwrap().to_string(), original);
    }
}

#[test]
fn unsupported_fields_and_unbounded_strings_fail_generation() {
    let mut data: Value =
        serde_json::from_str(include_str!("../../../chunk-protocol/data/26.2/protocol.json")).unwrap();
    data["handshaking"]["toServer"]["types"]["packet_set_protocol"][1][0]["type"] = "unknown".into();
    assert!(generate_packet(&data, &PACKETS[0]).unwrap_err().to_string().contains("unsupported wire type"));
    data["handshaking"]["toServer"]["types"]["packet_set_protocol"][1][0]["type"] = "string".into();
    assert!(generate(&data).unwrap_err().to_string().contains("protocolVersion: missing limit"));
}

#[test]
fn unsupported_nested_shapes_report_packet_and_field() {
    use serde_json::json;

    let original: Value =
        serde_json::from_str(include_str!("../../../chunk-protocol/data/26.2/protocol.json")).unwrap();
    for schema in [
        json!(["buffer", {"countType": "i32"}]),
        json!(["buffer", {"countType": "varint", "count": 8}]),
        json!(["switch", {"compareTo": "other", "fields": {}}]),
        json!(["option", "unknown"]),
        json!(["array", {"countType": "varint", "type": ["container", [{"name": "bytes", "type": "restBuffer"}]]}]),
    ] {
        let mut data = original.clone();
        data["login"]["toClient"]["types"]["packet_encryption_begin"][1][1]["type"] = schema;
        let error = generate(&data).unwrap_err().to_string();
        assert!(error.contains("login.toClient.encryption_begin: publicKey:"), "{error}");
    }
    let mut data = original;
    data["login"]["toClient"]["types"]["packet_encryption_begin"][1][1]["type"] = json!("restBuffer");
    assert!(generate(&data).unwrap_err().to_string().contains("restBuffer must be the last field"));
}
