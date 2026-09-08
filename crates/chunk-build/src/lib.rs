//! Immutable local backend bundles and gameplay classpaths.

mod program;
pub use program::pin_program;
mod codegen;
pub use codegen::generate;
mod compiler;
pub use compiler::compile;

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use chunk_contract::{DatabaseSchema, Deployment, Function, RuntimeProfile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
    fs::create_dir_all(directory)?;
    let destination = directory.join(&id);
    if destination.exists() {
        verify(&destination, &files)?;
    } else {
        let staging = tempfile::Builder::new().prefix(".build-").tempdir_in(directory)?;
        for (name, bytes) in &files {
            let path = staging.path().join(name);
            fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("artifact path"))?)?;
            let mut file = fs::File::options().create_new(true).write(true).open(&path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions)?;
        }
        match fs::rename(staging.path(), &destination) {
            Ok(()) => {}
            Err(error) if destination.exists() => {
                verify(&destination, &files).map_err(|_| error)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(Artifact {
        id,
        directory: destination.canonicalize()?,
    })
}

fn collect(directory: &Path, prefix: &str, files: &mut BTreeMap<String, Vec<u8>>) -> io::Result<()> {
    if prefix.matches('/').count() > 16 {
        return Err(io::Error::other("artifact directory nesting exceeds local limit"));
    }
    if fs::symlink_metadata(directory)?.file_type().is_symlink() {
        return Err(io::Error::other("artifact symlinks are unsupported"));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let filename = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("artifact filenames must be UTF-8"))?;
        let name = if prefix.is_empty() {
            filename
        } else {
            format!("{prefix}/{filename}")
        };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), &name, files)?;
        } else if kind.is_file() {
            if files.len() >= 4096 {
                return Err(io::Error::other("artifact exceeds local size limits"));
            }
            let bytes = read_limited(&entry.path(), 128 * 1024 * 1024)?;
            if files.values().map(Vec::len).sum::<usize>() + bytes.len() > 256 * 1024 * 1024 {
                return Err(io::Error::other("artifact exceeds local size limits"));
            }
            files.insert(name, bytes);
        } else {
            return Err(io::Error::other("artifact contains a non-regular file"));
        }
    }
    Ok(())
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::other("artifact requires regular files"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("artifact file exceeds local limit"));
    }
    Ok(bytes)
}

fn digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    for (name, bytes) in files {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

fn verify(directory: &Path, expected: &BTreeMap<String, Vec<u8>>) -> io::Result<()> {
    let mut actual = BTreeMap::new();
    collect(directory, "", &mut actual)?;
    if actual != *expected {
        return Err(io::Error::other("published artifact was modified"));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
