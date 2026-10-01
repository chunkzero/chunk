//! Backend compilation, generated clients and immutable application releases.

mod codegen;
pub use codegen::{GenerationTarget, generate};
#[cfg(feature = "compiler")]
mod compiler;
#[cfg(feature = "compiler")]
pub use compiler::{compile, compile_with};
#[cfg(feature = "compiler")]
pub mod migrations;
#[cfg(feature = "compiler")]
pub mod project;
mod publication;
mod release;
pub use release::{
    ArchiveDigest, Installed, JavaRuntime, JvmDescriptor, UnpackLimits, VerifiedRelease, install_release,
    install_trusted_release, installed_release, read_jvm_descriptor, unpack_release, verify_release,
};
#[cfg(feature = "compiler")]
pub use release::{Release, ReleaseInputs, publish_release};
#[cfg(feature = "compiler")]
mod sdk;
#[cfg(feature = "compiler")]
pub use sdk::generate_sdk;

use std::collections::BTreeMap;

use chunk_contract::{Contracts, DatabaseSchema, Function, RuntimeProfile};
use publication::read_limited;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackendMetadata {
    contract_version: u32,
    runtime_profile: RuntimeProfile,
    tables: DatabaseSchema,
    functions: BTreeMap<String, Function>,
    #[serde(flatten)]
    contracts: Contracts,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineProfile {
    pub memory_mib: u32,
    pub max_sessions: u16,
}

impl MachineProfile {
    pub(crate) fn valid(name: &str, profile: &Self) -> bool {
        !name.is_empty()
            && name.len() <= 128
            && (128..=8192).contains(&profile.memory_mib)
            && (1..=16).contains(&profile.max_sessions)
    }
}

pub(crate) fn valid_id(id: &str) -> bool {
    id.len() <= 128
        && id.bytes().next().is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// A JSON string literal, which is also a valid JavaScript and TypeScript string literal.
fn quote(value: impl AsRef<str>) -> String {
    serde_json::to_string(value.as_ref()).expect("string serialization")
}

#[cfg(test)]
mod tests;
