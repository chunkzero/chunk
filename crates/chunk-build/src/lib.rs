//! Backend compilation, generated clients and immutable application releases.

mod program;
pub use program::pin_program;
mod codegen;
pub use codegen::{GenerationTarget, generate};
mod compiler;
pub use compiler::compile;
pub mod project;
mod publication;
mod release;
pub use release::{JavaRuntime, JvmDescriptor, Release, ReleaseInputs, publish_release, read_jvm_descriptor};
mod sdk;
pub use sdk::generate_sdk;

use std::collections::BTreeMap;

use chunk_contract::{DatabaseSchema, Function, RuntimeProfile};
use publication::read_limited;
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackendMetadata {
    contract_version: u32,
    runtime_profile: RuntimeProfile,
    tables: DatabaseSchema,
    functions: BTreeMap<String, Function>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_methods: Option<chunk_contract::SessionMethods>,
}

#[cfg(test)]
mod tests;
