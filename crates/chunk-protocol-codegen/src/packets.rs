use std::fmt::Write;

use serde_json::Value;

use super::{Result, string};

mod schema;
mod specs;

use specs::{PACKETS, PacketSpec};

/// Packets `chunk_protocol` encodes directly: state, direction, source, ID constant and the pinned schema its codec
/// implements.
const DIRECT: &[(&str, &str, &str, &str, &str)] = &[(
    "configuration",
    "toClient",
    "add_resource_pack",
    "ADD_RESOURCE_PACK_ID",
    r#"["container",[{"name":"uuid","type":"UUID"},{"name":"url","type":"string"},{"name":"hash","type":"string"},{"name":"forced","type":"bool"},{"name":"promptMessage","type":["option","anonymousNbt"]}]]"#,
)];

pub(super) fn generate(protocol: &Value) -> Result<proc_macro2::TokenStream> {
    let mut output = String::new();
    for packet in PACKETS {
        output.push_str(
            &generate_packet(protocol, packet)
                .map_err(|error| format!("{}.{}.{}: {error}", packet.state, packet.direction, packet.source))?,
        );
    }
    for (state, direction, source, constant, schema) in DIRECT {
        if *definition(protocol, state, direction, source)? != serde_json::from_str::<Value>(schema)? {
            return Err(format!("{state}.{direction}.{source}: schema changed; review its direct codec").into());
        }
        let id = packet_id(protocol, state, direction, source)?;
        writeln!(output, "\npub const {constant}: i32 = {id:#04x};")?;
    }
    Ok(output.parse()?)
}

/// The payload schema of `source`'s packet.
fn definition<'a>(protocol: &'a Value, state: &str, direction: &str, source: &str) -> Result<&'a Value> {
    let types = &protocol[state][direction]["types"];
    let dispatch = tagged(&types["packet"], "container")?.as_array().ok_or("missing dispatch fields")?;
    let params = dispatch.iter().find(|field| field["name"] == "params").ok_or("missing packet payload switch")?;
    let switch = tagged(&params["type"], "switch")?;
    if switch["compareTo"] != "name" {
        return Err("packet switch must use name".into());
    }
    let type_name = string(&switch["fields"][source])?;
    types
        .get(type_name)
        .or_else(|| protocol["types"].get(type_name))
        .ok_or_else(|| format!("missing packet type {type_name}").into())
}

fn tagged<'a>(value: &'a Value, tag: &str) -> Result<&'a Value> {
    let parts = value.as_array().ok_or("expected tagged schema type")?;
    if parts.len() != 2 || string(&parts[0])? != tag {
        return Err(format!("expected {tag} schema, got {value}").into());
    }
    Ok(&parts[1])
}

pub(super) fn packet_id(protocol: &Value, state: &str, direction: &str, source: &str) -> Result<i32> {
    let types = &protocol[state][direction]["types"];
    let dispatch = tagged(&types["packet"], "container")?.as_array().ok_or("missing dispatch fields")?;
    let name_field = dispatch.iter().find(|field| field["name"] == "name").ok_or("missing packet name mapper")?;
    let mapper = tagged(&name_field["type"], "mapper")?;
    if mapper["type"] != "varint" {
        return Err("packet IDs must be VarInts".into());
    }
    let mappings = mapper["mappings"].as_object().ok_or("missing packet ID mappings")?;
    let ids: Vec<_> = mappings.iter().filter(|(_, name)| name.as_str() == Some(source)).collect();
    let [(wire_id, _)] = ids.as_slice() else {
        return Err(format!("expected one packet ID for {source}").into());
    };
    let id =
        if let Some(hex) = wire_id.strip_prefix("0x") { i32::from_str_radix(hex, 16)? } else { wire_id.parse()? };
    if id < 0 {
        return Err("packet ID must be nonnegative".into());
    }
    Ok(id)
}

fn generate_packet(protocol: &Value, spec: &PacketSpec) -> Result<String> {
    let id = packet_id(protocol, spec.state, spec.direction, spec.source)?;
    let definition = definition(protocol, spec.state, spec.direction, spec.source)?;
    let fields = tagged(definition, "container")?.as_array().ok_or("missing packet fields")?;
    let state = match spec.state {
        "handshaking" => "Handshake",
        "status" => "Status",
        "login" => "Login",
        "configuration" => "Configuration",
        "play" => "Play",
        _ => return Err("unsupported packet state".into()),
    };
    let direction = match spec.direction {
        "toServer" => "Serverbound",
        "toClient" => "Clientbound",
        _ => return Err("unsupported packet direction".into()),
    };
    let eq = if fields.iter().any(|field| schema::contains_float(&field["type"])) { "" } else { "Eq," };
    let mut output = format!(
        "\n#[derive(Debug, Clone, PartialEq, {eq} Encode, Decode, Packet)]\n#[packet(id = {id:#04x}, state = {state}, direction = {direction})]\npub struct {}",
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
mod tests;
