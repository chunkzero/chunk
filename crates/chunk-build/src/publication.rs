use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

pub(super) type Files = BTreeMap<String, Vec<u8>>;
const MAX_BYTES: usize = 256 * 1024 * 1024;
const MAX_FILES: usize = 4096;

pub(super) fn insert(files: &mut Files, name: String, bytes: Vec<u8>) -> io::Result<()> {
    relative_name(&name)?;
    if let Some(previous) = files.get(&name) {
        if previous != &bytes {
            return Err(io::Error::other(format!("conflicting artifact path {name}")));
        }
        return Ok(());
    }
    if files.len() >= MAX_FILES || files.values().map(Vec::len).sum::<usize>() + bytes.len() > MAX_BYTES {
        return Err(io::Error::other("artifact exceeds local size limits"));
    }
    files.insert(name, bytes);
    Ok(())
}

fn relative_name(name: &str) -> io::Result<()> {
    if name.chars().any(|ch| ch.is_control() || "\\:*?\"<>|".contains(ch))
        || name.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']))
    {
        return Err(io::Error::other(format!("artifact path is not portable: {name:?}")));
    }
    Ok(())
}

pub(super) fn collect(directory: &Path, prefix: &str, files: &mut Files) -> io::Result<()> {
    if prefix.matches('/').count() > 16 {
        return Err(io::Error::other("artifact directory nesting exceeds local limit"));
    }
    if !fs::symlink_metadata(directory)?.is_dir() {
        return Err(io::Error::other("artifact requires directories, not symlinks"));
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let filename =
            entry.file_name().into_string().map_err(|_| io::Error::other("artifact filenames must be UTF-8"))?;
        let name = if prefix.is_empty() { filename } else { format!("{prefix}/{filename}") };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(&entry.path(), &name, files)?;
        } else if kind.is_file() {
            insert(files, name, read_limited(&entry.path(), 128 * 1024 * 1024)?)?;
        } else {
            return Err(io::Error::other("artifact contains a non-regular file"));
        }
    }
    Ok(())
}

pub(super) fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(format!("{} requires a regular file", path.display())));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::other("artifact file exceeds local limit"));
    }
    Ok(bytes)
}

pub(super) fn digest(files: &Files) -> String {
    let mut digest = Sha256::new();
    for (name, bytes) in files {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

pub(super) fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

pub(super) fn publish_directory(directory: &Path, id: &str, files: &Files) -> io::Result<PathBuf> {
    if files.len() > MAX_FILES || files.values().map(Vec::len).sum::<usize>() > MAX_BYTES {
        return Err(io::Error::other("artifact exceeds local size limits"));
    }
    fs::create_dir_all(directory)?;
    let destination = directory.join(id);
    if exists(&destination)? {
        verify(&destination, files)?;
    } else {
        let staging = tempfile::Builder::new().prefix(".build-").tempdir_in(directory)?;
        for (name, bytes) in files {
            relative_name(name)?;
            let path = staging.path().join(name);
            fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("artifact path"))?)?;
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

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
fn rename_directory(source: &Path, destination: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
fn rename_directory(source: &Path, destination: &Path) -> io::Result<()> {
    // Windows directory rename fails if the destination already exists.
    fs::rename(source, destination)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
fn rename_directory(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "atomic directory publication is unsupported on this OS"))
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
