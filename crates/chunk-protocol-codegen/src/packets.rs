use serde_json::Value;

use super::{Result, string};

mod schema;
mod specs;

use specs::{PACKETS, PacketSpec};

pub(super) fn generate(protocol: &Value) -> Result<proc_macro2::TokenStream> {
    let mut output = String::new();
    for packet in PACKETS {
        output.push_str(
            &generate_packet(protocol, packet)
                .map_err(|error| format!("{}.{}.{}: {error}", packet.state, packet.direction, packet.source))?,
        );
    }
    Ok(output.parse()?)
}

fn tagged<'a>(value: &'a Value, tag: &str) -> Result<&'a Value> {
    let parts = value.as_array().ok_or("expected tagged schema type")?;
    if parts.len() != 2 || string(&parts[0])? != tag {
        return Err(format!("expected {tag} schema, got {value}").into());
    }
    Ok(&parts[1])
}

fn generate_packet(protocol: &Value, spec: &PacketSpec) -> Result<String> {
    let types = &protocol[spec.state][spec.direction]["types"];
    let dispatch = tagged(&types["packet"], "container")?
        .as_array()
        .ok_or("missing dispatch fields")?;
    let name_field = dispatch
        .iter()
        .find(|field| field["name"] == "name")
        .ok_or("missing packet name mapper")?;
    let mapper = tagged(&name_field["type"], "mapper")?;
    if mapper["type"] != "varint" {
        return Err("packet IDs must be VarInts".into());
    }
    let mappings = mapper["mappings"].as_object().ok_or("missing packet ID mappings")?;
    let ids: Vec<_> = mappings
        .iter()
        .filter(|(_, name)| name.as_str() == Some(spec.source))
        .collect();
    let [(wire_id, _)] = ids.as_slice() else {
        return Err(format!("expected one packet ID for {}", spec.source).into());
    };
    let id = if let Some(hex) = wire_id.strip_prefix("0x") {
        i32::from_str_radix(hex, 16)?
    } else {
        wire_id.parse()?
    };
    if id < 0 {
        return Err("packet ID must be nonnegative".into());
    }
    let params = dispatch
        .iter()
        .find(|field| field["name"] == "params")
        .ok_or("missing packet payload switch")?;
    let switch = tagged(&params["type"], "switch")?;
    if switch["compareTo"] != "name" {
        return Err("packet switch must use name".into());
    }
    let type_name = string(&switch["fields"][spec.source])?;
    let definition = types
        .get(type_name)
        .or_else(|| protocol["types"].get(type_name))
        .ok_or_else(|| format!("missing packet type {type_name}"))?;
    let fields = tagged(definition, "container")?
        .as_array()
        .ok_or("missing packet fields")?;
    let state = match spec.state {
        "handshaking" => "Handshake",
        "status" => "Status",
        "login" => "Login",
        "configuration" => "Configuration",
        _ => return Err("unsupported packet state".into()),
    };
    let direction = match spec.direction {
        "toServer" => "Serverbound",
        "toClient" => "Clientbound",
        _ => return Err("unsupported packet direction".into()),
    };
    let mut output = format!(
        "\n#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, Packet)]\n#[packet(id = {id:#04x}, state = {state}, direction = {direction})]\npub struct {}",
        spec.name
    );
    let mut definitions = String::new();
    if fields.is_empty() {
        output.push_str(";\n");
    } else {
        output.push_str(" {\n");
        output.push_str(&schema::fields(spec, fields, "", spec.name, &mut definitions)?);
        output.push_str("}\n");
    }
    output.push_str(&definitions);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_fields_and_unbounded_strings_fail_generation() {
        let mut data: Value =
            serde_json::from_str(include_str!("../../chunk-protocol/data/26.1/protocol.json")).unwrap();
        data["handshaking"]["toServer"]["types"]["packet_set_protocol"][1][0]["type"] = "unknown".into();
        assert!(
            generate_packet(&data, &PACKETS[0])
                .unwrap_err()
                .to_string()
                .contains("unsupported wire type")
        );
        data["handshaking"]["toServer"]["types"]["packet_set_protocol"][1][0]["type"] = "string".into();
        assert!(
            generate(&data)
                .unwrap_err()
                .to_string()
                .contains("protocolVersion: missing limit")
        );
    }

    #[test]
    fn unsupported_nested_shapes_report_packet_and_field() {
        use serde_json::json;

        let original: Value =
            serde_json::from_str(include_str!("../../chunk-protocol/data/26.1/protocol.json")).unwrap();
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
        assert!(
            generate(&data)
                .unwrap_err()
                .to_string()
                .contains("restBuffer must be the last field")
        );
    }
}
