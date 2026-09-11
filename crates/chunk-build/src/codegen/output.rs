use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Component, Path},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const MANIFEST: &str = ".chunk-codegen.json";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    files: BTreeMap<String, String>,
}

pub(super) fn write(output: &Path, files: &BTreeMap<String, String>) -> io::Result<()> {
    let previous = read_manifest(output)?;
    let paths = previous.keys().chain(files.keys()).collect::<BTreeSet<_>>();
    for name in paths {
        check_path(output, name)?;
        let path = output.join(name);
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                let Some(expected) = previous.get(name) else {
                    return Err(conflict(&path, "refusing to overwrite an unowned file"));
                };
                let bytes = crate::read_limited(&path, 16 * 1024 * 1024)?;
                if digest(&bytes) != *expected {
                    return Err(conflict(&path, "generated file was modified; move it before regenerating"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::create_dir_all(output)?;
    let manifest = Manifest {
        version: 1,
        files: files.iter().map(|(name, source)| (name.clone(), digest(source.as_bytes()))).collect(),
    };
    // Prepare every source before replacing the previous output and its ownership record.
    let staging = tempfile::Builder::new().prefix(".codegen-").tempdir_in(output)?;
    for (name, source) in files {
        let path = staging.path().join(name);
        fs::create_dir_all(path.parent().expect("validated relative file"))?;
        fs::write(path, source)?;
    }
    fs::write(staging.path().join(MANIFEST), serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?)?;
    for name in previous.keys().filter(|name| !files.contains_key(*name)) {
        match fs::remove_file(output.join(name)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    for name in files.keys() {
        let path = output.join(name);
        fs::create_dir_all(path.parent().expect("validated relative file"))?;
        fs::rename(staging.path().join(name), path)?;
    }
    fs::rename(staging.path().join(MANIFEST), output.join(MANIFEST))
}

fn read_manifest(output: &Path) -> io::Result<BTreeMap<String, String>> {
    match fs::symlink_metadata(output) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(conflict(output, "expected a directory, not a symlink or file"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
        _ => {}
    }
    let path = output.join(MANIFEST);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error),
        _ => {}
    }
    let manifest: Manifest =
        serde_json::from_slice(&crate::read_limited(&path, 512 * 1024)?).map_err(|error| conflict(&path, error))?;
    if manifest.version != 1 {
        return Err(conflict(&path, "unsupported generator ownership version"));
    }
    Ok(manifest.files)
}

fn check_path(output: &Path, name: &str) -> io::Result<()> {
    let relative = Path::new(name);
    if name.is_empty()
        || name == MANIFEST
        || name.contains('\\')
        || name.split('/').any(|part| part.is_empty() || part == "." || part == "..")
        || relative.components().any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(conflict(relative, "invalid generated relative path"));
    }
    let mut path = output.to_path_buf();
    for component in relative.components() {
        path.push(component);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_symlink() => {
                return Err(conflict(&path, "generated paths cannot traverse symlinks"));
            }
            Ok(metadata) if path != output.join(relative) && !metadata.is_dir() => {
                return Err(conflict(&path, "expected a generated source directory"));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
            _ => {}
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn conflict(path: &Path, message: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{}: {message}", path.display()))
}
