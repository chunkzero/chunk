use std::fmt::Write as _;

use serde_json::Value;

use super::{PacketSpec, Result, string, tagged};

pub(super) fn fields(
    spec: &PacketSpec,
    fields: &[Value],
    path: &str,
    parent: &str,
    definitions: &mut String,
) -> Result<String> {
    let mut output = String::new();
    let mut names = std::collections::HashSet::new();
    for (index, field) in fields.iter().enumerate() {
        if field.as_object().is_none_or(|field| field.len() != 2) {
            return Err(format!("unsupported container field {field}").into());
        }
        let source = string(&field["name"])?;
        let path = if path.is_empty() {
            source.into()
        } else {
            format!("{path}.{source}")
        };
        let rename = spec
            .fields
            .iter()
            .find(|field| field.path == path)
            .and_then(|field| field.rename);
        let name = field_name(rename.unwrap_or(source))?;
        if !names.insert(name.clone()) {
            return Err(format!("duplicate field name {name}").into());
        }
        let ty = wire_type(
            spec,
            &path,
            &field["type"],
            &format!("{parent}{}", pascal_case(&name)),
            definitions,
        )
        .map_err(|error| format!("{path}: {error}"))?;
        if consumes_remainder(&field["type"]) && index + 1 != fields.len() {
            return Err(format!("{path}: restBuffer must be the last field").into());
        }
        writeln!(output, "pub {name}: {ty},")?;
    }
    Ok(output)
}

fn consumes_remainder(schema: &Value) -> bool {
    match schema[0].as_str() {
        Some("option") => consumes_remainder(&schema[1]),
        Some("container") => schema[1]
            .as_array()
            .is_some_and(|fields| fields.iter().any(|field| consumes_remainder(&field["type"]))),
        Some("array") => consumes_remainder(&schema[1]["type"]),
        _ => schema == "restBuffer",
    }
}

fn limit(spec: &PacketSpec, path: &str) -> Result<usize> {
    spec.fields
        .iter()
        .find(|field| field.path == path)
        .and_then(|field| field.limit)
        .ok_or_else(|| "missing limit".into())
}

fn wire_type(spec: &PacketSpec, path: &str, schema: &Value, name: &str, definitions: &mut String) -> Result<String> {
    if let Some(primitive) = schema.as_str() {
        return Ok(match primitive {
            "varint" => "VarInt".into(),
            "u8" | "i8" | "u16" | "i32" | "u32" | "i64" | "f32" | "f64" | "bool" => primitive.into(),
            "MovementFlags" => "u8".into(),
            "PositionUpdateRelatives" => "u32".into(),
            "UUID" => "Uuid".into(),
            "string" => format!("McString<{}>", limit(spec, path)?),
            "restBuffer" => format!("RemainingBytes<{}>", limit(spec, path)?),
            other => return Err(format!("unsupported wire type {other}").into()),
        });
    }
    let parts = schema
        .as_array()
        .filter(|parts| parts.len() == 2)
        .ok_or("expected a two-part schema type")?;
    let tag = string(&parts[0])?;
    let args = &parts[1];
    match tag {
        "buffer" if args.as_object().is_some_and(|args| args.len() == 1) && args["countType"] == "varint" => {
            Ok(format!("ByteArray<{}>", limit(spec, path)?))
        }
        "option" => Ok(format!("Option<{}>", wire_type(spec, path, args, name, definitions)?)),
        "array" if args.as_object().is_some_and(|args| args.len() == 2) && args["countType"] == "varint" => {
            let count = limit(spec, path)?;
            if consumes_remainder(&args["type"]) {
                return Err("restBuffer cannot be an array element".into());
            }
            let element_path = if args["type"][0] == "container" {
                path.into()
            } else {
                format!("{path}[]")
            };
            let element = wire_type(spec, &element_path, &args["type"], &format!("{name}Entry"), definitions)?;
            Ok(format!("BoundedArray<{element}, {count}>"))
        }
        "container" => {
            let body = fields(
                spec,
                args.as_array().ok_or("expected container fields")?,
                path,
                name,
                definitions,
            )?;
            writeln!(
                definitions,
                "#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)] pub struct {name} {{ {body} }}"
            )?;
            Ok(name.into())
        }
        "mapper" => mapper(schema, name, definitions),
        _ => Err(format!("unsupported schema shape {schema}").into()),
    }
}

fn mapper(schema: &Value, name: &str, definitions: &mut String) -> Result<String> {
    let args = tagged(schema, "mapper")?;
    if args["type"] != "varint" || args.as_object().is_none_or(|args| args.len() != 2) {
        return Err("only VarInt enum mappers are supported".into());
    }
    let mappings = args["mappings"]
        .as_object()
        .filter(|entries| !entries.is_empty())
        .ok_or("missing enum mappings")?;
    let mut variants = String::new();
    let mut encode = String::new();
    let mut decode = String::new();
    let mut names = std::collections::HashSet::new();
    let mut ids = std::collections::HashSet::new();
    for (id, label) in mappings {
        let id: i32 = id.parse()?;
        let variant = pascal_case(&field_name(string(label)?)?);
        syn::parse_str::<syn::Ident>(&variant)?;
        if !names.insert(variant.clone()) || !ids.insert(id) {
            return Err("duplicate enum variant or ID".into());
        }
        write!(variants, "{variant},")?;
        write!(encode, "Self::{variant} => {id},")?;
        write!(decode, "{id} => Ok(Self::{variant}),")?;
    }
    write!(
        definitions,
        "
        #[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum {name} {{ {variants} }}
        impl Encode for {name} {{
            fn encode(&self, output: &mut Vec<u8>) -> ::chunk_protocol::Result<()> {{
                VarInt(match self {{ {encode} }}).encode(output)
            }}
        }}
        impl Decode for {name} {{
            fn decode(input: &mut &[u8]) -> ::chunk_protocol::Result<Self> {{
                match VarInt::decode(input)?.0 {{ {decode} _ => Err(::chunk_protocol::Error::InvalidEnumValue) }}
            }}
        }}
    "
    )?;
    Ok(name.into())
}

fn field_name(source: &str) -> Result<String> {
    let mut name = String::new();
    for character in source.chars() {
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
    syn::parse_str::<syn::Ident>(&name).map_err(|_| format!("unsupported Rust field name {name}"))?;
    Ok(name)
}

fn pascal_case(name: &str) -> String {
    let mut output = String::new();
    for part in name.split('_').filter(|part| !part.is_empty()) {
        let mut chars = part.chars();
        output.push(chars.next().expect("nonempty part").to_ascii_uppercase());
        output.push_str(chars.as_str());
    }
    output
}
