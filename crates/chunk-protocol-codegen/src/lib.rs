//! Generates public protocol modules from pinned local `PrismarineJS` datasets.

mod commands;
mod packets;
mod registries;

use std::{collections::BTreeMap, error::Error, fs, path::Path};

use proc_macro::TokenStream;
use proc_macro2::TokenStream as Tokens;
use quote::quote;
use serde_json::Value;
use sha2::{Digest, Sha256};
use syn::{
    Ident, LitStr, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

struct Invocation {
    module: Ident,
    directory: LitStr,
}

impl Parse for Invocation {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let module = input.parse()?;
        input.parse::<Token![,]>()?;
        let directory = input.parse()?;
        if !input.is_empty() {
            input.parse::<Token![,]>()?;
        }
        Ok(Self { module, directory })
    }
}

/// Generates a public protocol module from a dataset directory relative to
/// the invoking crate's `Cargo.toml`.
#[proc_macro]
pub fn protocol_version(input: TokenStream) -> TokenStream {
    let invocation = parse_macro_input!(input as Invocation);
    expand(&invocation)
        .unwrap_or_else(|error| syn::Error::new(invocation.directory.span(), error.to_string()).into_compile_error())
        .into()
}

fn expand(invocation: &Invocation) -> Result<Tokens> {
    let root = std::env::var_os("CARGO_MANIFEST_DIR").ok_or("Cargo did not set CARGO_MANIFEST_DIR")?;
    let relative = invocation.directory.value();
    if Path::new(&relative).is_absolute() {
        return Err("dataset directory must be relative to the invoking crate's manifest".into());
    }
    let directory = Path::new(&root).join(relative).canonicalize()?;
    let source_path = directory.join("source.json");
    let source: Value = serde_json::from_slice(&fs::read(&source_path)?)?;
    let entries = source["files"].as_object().ok_or("missing source files")?;
    let mut inputs = BTreeMap::new();
    let mut dependencies = vec![source_path];
    for (name, entry) in entries {
        let path = directory.join(name);
        let contents = fs::read(&path)?;
        let hash = format!("{:x}", Sha256::digest(&contents));
        if hash != string(&entry["sha256"])? {
            return Err(format!("upstream snapshot checksum mismatch: {name}").into());
        }
        dependencies.push(path);
        inputs.insert(name.as_str(), contents);
    }
    let protocol: Value =
        serde_json::from_slice(inputs.get("protocol.json").ok_or("protocol.json must be pinned in source.json")?)?;
    let version: Value =
        serde_json::from_slice(inputs.get("version.json").ok_or("version.json must be pinned in source.json")?)?;
    let version_name = string(&version["minecraftVersion"])?;
    if version_name != "26.1" || string(&version["releaseType"])? != "release" {
        return Err("this generator's selected packets are validated for release 26.1".into());
    }
    let protocol_id = i32::try_from(version["version"].as_i64().ok_or("missing protocol version")?)?;
    let packets = packets::generate(&protocol)?;
    let commands = commands::generate(&protocol)?;
    let registries =
        registries::generate(&protocol, inputs.get("loginPacket.json").ok_or("missing loginPacket.json")?)?;
    let module = &invocation.module;
    let documentation = format!(
        "Java Edition {version_name}; selected handshake, status, login, configuration and play packets. Generated from `PrismarineJS/minecraft-data` @ {} (MIT). Attribution accompanies the dataset.",
        string(&source["revision"])?
    );
    let dependencies = dependencies
        .iter()
        .map(|path| path.to_str().ok_or_else(|| "dataset paths must be valid UTF-8".into()))
        .collect::<Result<Vec<_>>>()?;
    Ok(quote! {
        #[doc = #documentation]
        pub mod #module {
            // Track dataset changes without exposing their bytes in the public API.
            #(const _: &[u8] = ::core::include_bytes!(#dependencies);)*

            use ::chunk_protocol::{BoundedArray, ByteArray, Decode, Encode, McString, Packet, RemainingBytes, Uuid, VarInt};

            pub const VERSION: ::chunk_protocol::versions::Version = ::chunk_protocol::versions::Version {
                name: #version_name,
                protocol: #protocol_id,
            };

            #packets
            #commands
            #registries
        }
    })
}

fn string(value: &Value) -> Result<&str> {
    value.as_str().ok_or_else(|| format!("expected schema string, got {value}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invocation_requires_a_module_identifier_and_literal_path() {
        assert!(syn::parse_str::<Invocation>("v26_1, \"data/26.1\"").is_ok());
        assert!(syn::parse_str::<Invocation>("26_1, \"data/26.1\"").is_err());
        assert!(syn::parse_str::<Invocation>("v26_1, path").is_err());
        assert!(syn::parse_str::<Invocation>("v26_1, \"data/26.1\", extra").is_err());
    }
}
