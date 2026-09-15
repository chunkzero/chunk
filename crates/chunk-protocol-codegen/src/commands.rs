use proc_macro2::TokenStream;
use quote::quote;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{Result, packets::packet_id, string};

// Direct codecs below implement these pinned ProtoDef switches, not arbitrary future shapes.
const SHAPE: &str = "2bd0e2e07af3e83139dfcc5deda2e92d0ce74f061bde1648f62ec2479b727a64";

pub(super) fn generate(protocol: &Value) -> Result<TokenStream> {
    let mut shapes = Map::from_iter([("command_node".into(), protocol["types"]["command_node"].clone())]);
    let mut constants = TokenStream::new();
    for (direction, name, constant) in [
        ("toServer", "chat_command_signed", "SIGNED_COMMAND_ID"),
        ("toClient", "declare_commands", "COMMAND_TREE_ID"),
        ("toClient", "tab_complete", "COMMAND_SUGGESTIONS_ID"),
        ("toClient", "system_chat", "SYSTEM_MESSAGE_ID"),
        ("toClient", "action_bar", "ACTION_BAR_ID"),
        ("toClient", "set_title_text", "TITLE_TEXT_ID"),
        ("toClient", "set_title_subtitle", "SUBTITLE_TEXT_ID"),
    ] {
        shapes.insert(
            format!("{direction}/{name}"),
            protocol["play"][direction]["types"][format!("packet_{name}")].clone(),
        );
        let id = packet_id(protocol, "play", direction, name)?;
        let constant = syn::Ident::new(constant, proc_macro2::Span::call_site());
        constants.extend(quote! { pub const #constant: i32 = #id; });
    }
    if format!("{:x}", Sha256::digest(serde_json::to_vec(&shapes)?)) != SHAPE {
        return Err("unsupported command packet or parser property schema; review the pinned direct codecs".into());
    }
    let mappings = protocol["types"]["command_node"][1][3]["type"][1]["fields"]["2"][1][1]["type"][1]["mappings"]
        .as_object()
        .ok_or("missing command parser mappings")?;
    let mut parsers = Vec::new();
    for (id, name) in mappings {
        let id: i32 = id.parse()?;
        let name = string(name)?;
        let kind = match name {
            "brigadier:float" | "brigadier:integer" => quote! { Numeric32 },
            "brigadier:double" | "brigadier:long" => quote! { Numeric64 },
            "brigadier:string" => quote! { StringMode },
            "minecraft:entity" => quote! { Entity },
            "minecraft:score_holder" => quote! { ScoreHolder },
            "minecraft:time" => quote! { Time },
            "minecraft:resource_or_tag"
            | "minecraft:resource_or_tag_key"
            | "minecraft:resource"
            | "minecraft:resource_key"
            | "minecraft:resource_selector" => quote! { Registry },
            _ => quote! { None },
        };
        parsers.push(quote! { (#id, #name, ::chunk_protocol::commands::PropertyKind::#kind) });
    }
    Ok(quote! {
        /// IDs and parser layouts for the bounded 26.1 command codecs.
        pub mod commands {
            #constants
            #[doc(hidden)]
            pub const PARSERS: &[(i32, &str, ::chunk_protocol::commands::PropertyKind)] = &[#(#parsers),*];
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packet_ids_follow_the_dataset_but_parser_shape_changes_fail_closed() {
        let mut data: Value =
            serde_json::from_str(include_str!("../../chunk-protocol/data/26.1/protocol.json")).unwrap();
        let original = generate(&data).unwrap().to_string();
        let mappings =
            data["play"]["toClient"]["types"]["packet"][1][0]["type"][1]["mappings"].as_object_mut().unwrap();
        let key = mappings.iter().find(|(_, name)| **name == "declare_commands").unwrap().0.clone();
        let value = mappings.remove(&key).unwrap();
        mappings.insert("0x100".into(), value);
        assert_ne!(generate(&data).unwrap().to_string(), original);
        data["types"]["command_node"][1][0]["type"][1][0]["size"] = 3.into();
        assert!(generate(&data).is_err());
    }
}
