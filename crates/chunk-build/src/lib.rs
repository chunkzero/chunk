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

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

use chunk_contract::{DatabaseSchema, Deployment, Function, RuntimeProfile};
use publication::{collect, digest, read_limited};
use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BackendMetadata {
    contract_version: u32,
    runtime_profile: RuntimeProfile,
    tables: DatabaseSchema,
    functions: BTreeMap<String, Function>,
}

pub struct Inputs {
    pub source: PathBuf,
    pub contract: PathBuf,
    pub distribution: PathBuf,
}

pub struct Artifact {
    pub id: String,
    pub directory: PathBuf,
}

/// Publishes a complete backend bundle and JVM classpath under their content digest.
/// Project metadata participates in identity; neither inputs nor an existing artifact are modified.
/// # Errors
/// Rejects invalid contracts, symlinks, oversized inputs, or a changed published artifact.
pub fn publish(inputs: &Inputs, directory: &Path, project: &[u8]) -> io::Result<Artifact> {
    let source = String::from_utf8(read_limited(&inputs.source, 4 * 1024 * 1024)?).map_err(io::Error::other)?;
    let contract: BackendMetadata =
        serde_json::from_slice(&read_limited(&inputs.contract, 2 * 1024 * 1024)?).map_err(io::Error::other)?;
    if contract.functions.is_empty() || project.len() > 65_536 {
        return Err(io::Error::other("invalid backend bundle inputs"));
    }
    let mut files = BTreeMap::new();
    collect(&inputs.distribution.join("lib"), "gameplay/lib", &mut files)?;
    if files.is_empty()
        || files
            .keys()
            .any(|name| Path::new(name).extension().is_none_or(|ext| ext != "jar") || name.matches('/').count() != 2)
    {
        return Err(io::Error::other(
            "gameplay distribution requires a lib directory of JARs",
        ));
    }
    files.insert("source.mjs".into(), source.as_bytes().to_vec());
    let source_map = inputs.source.with_extension("mjs.map");
    if source_map.exists() {
        files.insert("source.mjs.map".into(), read_limited(&source_map, 8 * 1024 * 1024)?);
    }
    files.insert(
        "contract.json".into(),
        serde_json::to_vec(&contract).map_err(io::Error::other)?,
    );
    files.insert("project.json".into(), project.to_vec());
    let id = digest(&files);
    let bundle = Deployment {
        contract_version: contract.contract_version,
        runtime_profile: contract.runtime_profile,
        id: id.clone(),
        source,
        tables: contract.tables,
        functions: contract.functions,
    };
    bundle.validate().map_err(io::Error::other)?;
    files.insert(
        "backend.json".into(),
        serde_json::to_vec(&bundle).map_err(io::Error::other)?,
    );
    let destination = publication::publish_directory(directory, &id, &files)?;
    Ok(Artifact {
        id,
        directory: destination,
    })
}

#[cfg(test)]
mod tests;
