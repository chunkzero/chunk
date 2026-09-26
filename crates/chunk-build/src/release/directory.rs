use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use crate::publication::{Files, MAX_BYTES, MAX_FILES, collect, read_limited, relative_name, rename_directory};

pub(super) fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn publish(directory: &Path, id: &str, files: &Files) -> io::Result<PathBuf> {
    if files.len() > MAX_FILES || files.values().map(Vec::len).sum::<usize>() > MAX_BYTES {
        return Err(io::Error::other("artifact exceeds local size limits"));
    }
    fs::create_dir_all(directory)?;
    let destination = directory.join(id);
    if exists(&destination)? {
        verify(&destination, files)?;
    } else {
        let staging = tempfile::Builder::new().prefix(".build-").tempdir_in(directory)?;
        let earlier = releases(directory)?;
        for (name, bytes) in files {
            relative_name(name)?;
            let path = staging.path().join(name);
            fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("artifact path"))?)?;
            if link_unchanged(&earlier, name, bytes, &path)? {
                continue;
            }
            let mut file = fs::File::options().create_new(true).write(true).open(&path)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            let mut permissions = file.metadata()?.permissions();
            permissions.set_readonly(true);
            file.set_permissions(permissions)?;
        }
        match rename_directory(staging.path(), &destination) {
            Ok(()) => {}
            Err(_) if exists(&destination)? => verify(&destination, files)?,
            Err(error) => return Err(error),
        }
    }
    destination.canonicalize()
}

/// Published release directories under `directory`, skipping staging directories and archives.
fn releases(directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut releases = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
            releases.push(entry.path());
        }
    }
    Ok(releases)
}

/// Hard-links `name` from the first earlier release holding it with identical bytes, so unchanged JARs are not
/// rewritten. Published files are read-only, and republishing a release still verifies its bytes.
fn link_unchanged(releases: &[PathBuf], name: &str, bytes: &[u8], path: &Path) -> io::Result<bool> {
    let Some(existing) = releases.iter().map(|release| release.join(name)).find(|existing| {
        fs::symlink_metadata(existing).is_ok_and(|metadata| metadata.is_file() && metadata.len() == bytes.len() as u64)
    }) else {
        return Ok(false);
    };
    Ok(read_limited(&existing, MAX_BYTES as u64)? == bytes && fs::hard_link(existing, path).is_ok())
}

fn verify(directory: &Path, expected: &Files) -> io::Result<()> {
    let mut actual = Files::new();
    collect(directory, "", &mut actual)?;
    if actual != *expected {
        return Err(io::Error::other("published artifact was modified"));
    }
    verify_directories(directory, "", expected)
}

fn verify_directories(directory: &Path, prefix: &str, expected: &Files) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let name = format!("{prefix}{}/", entry.file_name().to_string_lossy());
            if !expected.keys().any(|path| path.starts_with(&name)) {
                return Err(io::Error::other("published artifact was modified"));
            }
            verify_directories(&entry.path(), &name, expected)?;
        }
    }
    Ok(())
}
