use std::fmt::Write as _;

use serde_json::Value;

use super::{Result, string};

struct PacketSpec {
    state: &'static str,
    direction: &'static str,
    source: &'static str,
    name: &'static str,
}

const PACKETS: &[PacketSpec] = &[
    PacketSpec {
        state: "handshaking",
        direction: "toServer",
        source: "set_protocol",
        name: "Handshake",
    },
    PacketSpec {
        state: "status",
        direction: "toServer",
        source: "ping_start",
        name: "StatusRequest",
    },
    PacketSpec {
        state: "status",
        direction: "toClient",
        source: "server_info",
        name: "StatusResponse",
    },
    PacketSpec {
        state: "status",
        direction: "toServer",
        source: "ping",
        name: "Ping",
    },
    PacketSpec {
        state: "status",
        direction: "toClient",
        source: "ping",
        name: "Pong",
    },
    PacketSpec {
        state: "login",
        direction: "toClient",
        source: "disconnect",
        name: "LoginDisconnect",
    },
];

pub(super) fn generate(protocol: &Value) -> Result<proc_macro2::TokenStream> {
    let mut output = String::new();
    for packet in PACKETS {
        output.push_str(&generate_packet(protocol, packet)?);
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
    let fields = tagged(&types[type_name], "container")?
        .as_array()
        .ok_or("missing packet fields")?;
    let state = match spec.state {
        "handshaking" => "Handshake",
        "status" => "Status",
        "login" => "Login",
        _ => return Err("unsupported packet state".into()),
    };
    let direction = match spec.direction {
        "toServer" => "Serverbound",
        "toClient" => "Clientbound",
        _ => return Err("unsupported packet direction".into()),
    };
    let mut output = format!(
        "\n#[derive(Debug, Encode, Decode, Packet)]\n#[packet(id = {id:#04x}, state = {state}, direction = {direction})]\npub struct {}",
        spec.name
    );
    if fields.is_empty() {
        output.push_str(";\n");
    } else {
        output.push_str(" {\n");
        for field in fields {
            let source_name = string(&field["name"])?;
            let rust_name = field_name(source_name)?;
            let ty = field_type(spec, source_name, &field["type"])?;
            writeln!(output, "    pub {rust_name}: {ty},")?;
        }
        output.push_str("}\n");
    }
    Ok(output)
}

fn field_name(source: &str) -> Result<String> {
    // Map upstream field names to the public API.
    let rename = match source {
        "serverHost" => "server_address",
        "time" => "payload",
        "response" => "json",
        other => other,
    };
    let mut name = String::new();
    for character in rename.chars() {
        if character.is_ascii_uppercase() {
            name.push('_');
            name.push(character.to_ascii_lowercase());
        } else if character.is_ascii_lowercase() || character == '_' || (!name.is_empty() && character.is_ascii_digit())
        {
            name.push(character);
        } else {
            return Err(format!("unsupported field name {source}").into());
        }
    }
    if name.is_empty() {
        return Err("empty field name".into());
    }
    Ok(name)
}

fn field_type(spec: &PacketSpec, field: &str, schema: &Value) -> Result<String> {
    let ty = match string(schema)? {
        "varint" => "VarInt",
        "u16" => "u16",
        "i64" => "i64",
        "string" => {
            // ProtoDef omits these required per-field string limits.
            let limit = match (spec.state, spec.direction, spec.source, field) {
                ("handshaking", "toServer", "set_protocol", "serverHost") => 255,
                ("status", "toClient", "server_info", "response") | ("login", "toClient", "disconnect", "reason") => {
                    32767
                }
                _ => return Err(format!("missing string limit for {}.{field}", spec.source).into()),
            };
            return Ok(format!("McString<{limit}>"));
        }
        other => return Err(format!("unsupported wire type {other} in {}.{field}", spec.source).into()),
    };
    Ok(ty.into())
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
        assert!(field_type(&PACKETS[0], "newField", &Value::String("string".into())).is_err());
    }
}
