use proc_macro2::TokenStream;
use quote::quote;
use serde_json::Value;

use super::{Result, string};

const DAMAGE_TAGS: &[(&str, &[&str])] = &[
    (
        "minecraft:is_fire",
        &[
            "minecraft:in_fire",
            "minecraft:campfire",
            "minecraft:on_fire",
            "minecraft:lava",
            "minecraft:hot_floor",
            "minecraft:unattributed_fireball",
            "minecraft:fireball",
        ],
    ),
    (
        "minecraft:is_explosion",
        &[
            "minecraft:fireworks",
            "minecraft:explosion",
            "minecraft:player_explosion",
            "minecraft:bad_respawn_point",
        ],
    ),
    (
        "minecraft:bypasses_shield",
        &[
            "minecraft:on_fire",
            "minecraft:in_wall",
            "minecraft:cramming",
            "minecraft:drown",
            "minecraft:fly_into_wall",
            "minecraft:generic",
            "minecraft:wither",
            "minecraft:dragon_breath",
            "minecraft:starve",
            "minecraft:fall",
            "minecraft:ender_pearl",
            "minecraft:freeze",
            "minecraft:stalagmite",
            "minecraft:magic",
            "minecraft:indirect_magic",
            "minecraft:out_of_world",
            "minecraft:generic_kill",
            "minecraft:sonic_boom",
            "minecraft:outside_border",
            "minecraft:cactus",
            "minecraft:campfire",
            "minecraft:dry_out",
            "minecraft:falling_anvil",
            "minecraft:falling_stalactite",
            "minecraft:hot_floor",
            "minecraft:in_fire",
            "minecraft:lava",
            "minecraft:lightning_bolt",
            "minecraft:sweet_berry_bush",
        ],
    ),
];

// Only encodes trusted, checksum-verified upstream data at compile time.
pub(super) fn generate(bytes: &[u8]) -> Result<TokenStream> {
    let data = limbo_data(bytes)?;
    let registries = data["dimensionCodec"].as_object().ok_or("missing registries")?;
    let mut packets = Vec::new();
    for registry in registries.values() {
        let mut body = vec![0x07]; // Configuration Registry Data, protocol 775.
        text(string(&registry["id"])?, &mut body)?;
        let entries = registry["entries"].as_array().ok_or("missing entries")?;
        varint(entries.len(), &mut body)?;
        for entry in entries {
            text(string(&entry["key"])?, &mut body)?;
            body.push(1);
            let value = &entry["value"];
            body.push(tag(string(&value["type"])?)?);
            payload(string(&value["type"])?, &value["value"], &mut body)?;
        }
        let mut frame = Vec::new();
        varint(body.len(), &mut frame)?;
        frame.extend(body);
        packets.push(quote! { &[#(#frame),*] as &[u8] });
    }
    let biomes = registries["minecraft:worldgen/biome"]["entries"]
        .as_array()
        .ok_or("missing biomes")?;
    let end = i32::try_from(
        biomes
            .iter()
            .position(|entry| entry["key"] == "minecraft:the_end")
            .ok_or("missing End biome")?,
    )?;
    let tags = limbo_tags(registries)?;
    Ok(quote! {
        /// Framed limbo registries; unused enchantments and dialogs are empty.
        pub const LIMBO_REGISTRIES: &[&[u8]] = &[#(#packets),*];
        /// Framed tag bindings required by dimensions and client component initialization.
        pub const LIMBO_TAGS: &[u8] = &[#(#tags),*];
        pub const END_BIOME_ID: i32 = #end;
    })
}

fn limbo_data(bytes: &[u8]) -> Result<Value> {
    let mut data: Value = serde_json::from_slice(bytes)?;
    // Limbo has no enchanted items or dialogs. Their vanilla definitions depend
    // on gameplay tags that this world does not synchronize.
    for registry in ["minecraft:enchantment", "minecraft:dialog"] {
        data["dimensionCodec"][registry]["entries"]
            .as_array_mut()
            .ok_or("missing optional registry")?
            .clear();
    }
    Ok(data)
}

fn limbo_tags(registries: &serde_json::Map<String, Value>) -> Result<Vec<u8>> {
    let mut body = vec![0x0d]; // Configuration Update Tags.
    varint(3, &mut body)?;
    // Vanilla 26.1 bindings required by dimensions and item component initializers.
    // Nested tags are flattened; registry IDs are resolved from the pinned snapshot.
    append_tags(
        registries,
        "minecraft:timeline",
        &[
            (
                "minecraft:in_overworld",
                &[
                    "minecraft:villager_schedule",
                    "minecraft:day",
                    "minecraft:moon",
                    "minecraft:early_game",
                ],
            ),
            ("minecraft:in_nether", &["minecraft:villager_schedule"]),
            ("minecraft:in_end", &["minecraft:villager_schedule"]),
            ("minecraft:universal", &["minecraft:villager_schedule"]),
        ],
        &mut body,
    )?;
    append_tags(registries, "minecraft:damage_type", DAMAGE_TAGS, &mut body)?;
    append_tags(
        registries,
        "minecraft:banner_pattern",
        &[
            ("minecraft:pattern_item/bordure_indented", &["minecraft:curly_border"]),
            ("minecraft:pattern_item/creeper", &["minecraft:creeper"]),
            ("minecraft:pattern_item/field_masoned", &["minecraft:bricks"]),
            ("minecraft:pattern_item/flow", &["minecraft:flow"]),
            ("minecraft:pattern_item/flower", &["minecraft:flower"]),
            ("minecraft:pattern_item/globe", &["minecraft:globe"]),
            ("minecraft:pattern_item/guster", &["minecraft:guster"]),
            ("minecraft:pattern_item/mojang", &["minecraft:mojang"]),
            ("minecraft:pattern_item/piglin", &["minecraft:piglin"]),
            ("minecraft:pattern_item/skull", &["minecraft:skull"]),
        ],
        &mut body,
    )?;
    let mut frame = Vec::new();
    varint(body.len(), &mut frame)?;
    frame.extend(body);
    Ok(frame)
}

fn append_tags(
    registries: &serde_json::Map<String, Value>,
    registry: &str,
    tags: &[(&str, &[&str])],
    body: &mut Vec<u8>,
) -> Result<()> {
    let entries = registries[registry]["entries"]
        .as_array()
        .ok_or("missing tag registry")?;
    text(registry, body)?;
    varint(tags.len(), body)?;
    for (name, members) in tags {
        text(name, body)?;
        varint(members.len(), body)?;
        for member in *members {
            let id = entries
                .iter()
                .position(|entry| entry["key"] == *member)
                .ok_or_else(|| format!("missing tag member {registry}/{member}"))?;
            varint(id, body)?;
        }
    }
    Ok(())
}

fn varint(value: usize, output: &mut Vec<u8>) -> Result<()> {
    let mut value = u32::try_from(value)?;
    loop {
        let byte = u8::try_from(value & 127)?;
        value >>= 7;
        output.push(byte | if value == 0 { 0 } else { 128 });
        if value == 0 {
            return Ok(());
        }
    }
}

fn text(value: &str, output: &mut Vec<u8>) -> Result<()> {
    varint(value.len(), output)?;
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn nbt_text(value: &str, output: &mut Vec<u8>) -> Result<()> {
    // The snapshot's NBT strings are ASCII, also valid modified UTF-8.
    if !value.is_ascii() || value.contains('\0') {
        return Err("unsupported NBT string".into());
    }
    output.extend_from_slice(&u16::try_from(value.len())?.to_be_bytes());
    output.extend_from_slice(value.as_bytes());
    Ok(())
}

fn tag(name: &str) -> Result<u8> {
    Ok(match name {
        "byte" => 1,
        "short" => 2,
        "int" => 3,
        "long" => 4,
        "float" => 5,
        "double" => 6,
        "string" => 8,
        "list" => 9,
        "compound" => 10,
        "intArray" => 11,
        _ => return Err(format!("unsupported NBT tag {name}").into()),
    })
}

#[allow(clippy::cast_possible_truncation)] // NBT float values are explicitly IEEE f32.
fn payload(kind: &str, value: &Value, output: &mut Vec<u8>) -> Result<()> {
    match kind {
        "byte" => output.extend_from_slice(&i8::try_from(value.as_i64().ok_or("invalid byte")?)?.to_be_bytes()),
        "short" => output.extend_from_slice(&i16::try_from(value.as_i64().ok_or("invalid short")?)?.to_be_bytes()),
        "int" => output.extend_from_slice(&i32::try_from(value.as_i64().ok_or("invalid int")?)?.to_be_bytes()),
        "long" => {
            let parts = value.as_array().ok_or("invalid long")?;
            for part in parts {
                output.extend_from_slice(&i32::try_from(part.as_i64().ok_or("invalid long part")?)?.to_be_bytes());
            }
            if parts.len() != 2 {
                return Err("invalid long length".into());
            }
        }
        "float" => output.extend_from_slice(&(value.as_f64().ok_or("invalid float")? as f32).to_be_bytes()),
        "double" => output.extend_from_slice(&value.as_f64().ok_or("invalid double")?.to_be_bytes()),
        "string" => nbt_text(string(value)?, output)?,
        "intArray" => {
            let values = value.as_array().ok_or("invalid int array")?;
            output.extend_from_slice(&i32::try_from(values.len())?.to_be_bytes());
            for value in values {
                payload("int", value, output)?;
            }
        }
        "compound" => {
            for (name, child) in value.as_object().ok_or("invalid compound")? {
                let kind = string(&child["type"])?;
                output.push(tag(kind)?);
                nbt_text(name, output)?;
                payload(kind, &child["value"], output)?;
            }
            output.push(0);
        }
        "list" => {
            let kind = string(&value["type"])?;
            let values = value["value"].as_array().ok_or("invalid list")?;
            output.push(if values.is_empty() { 0 } else { tag(kind)? });
            output.extend_from_slice(&i32::try_from(values.len())?.to_be_bytes());
            for value in values {
                payload(kind, value, output)?;
            }
        }
        _ => return Err(format!("unsupported NBT payload {kind}").into()),
    }
    Ok(())
}
