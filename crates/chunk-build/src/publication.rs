use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::Path,
};

use sha2::{Digest, Sha256};

pub(super) type Files = BTreeMap<String, Vec<u8>>;
pub(super) const MAX_BYTES: usize = 256 * 1024 * 1024;
pub(super) const MAX_FILES: usize = 4096;
/// The most components and bytes an artifact path may have.
pub(super) const MAX_COMPONENTS: usize = 18;
pub(super) const MAX_PATH_BYTES: usize = 4096;

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

pub(super) fn relative_name(name: &str) -> io::Result<()> {
    if name.chars().any(|ch| ch.is_control() || "\\:*?\"<>|".contains(ch))
        || name.split('/').any(|part| part.is_empty() || part == "." || part == ".." || part.ends_with(['.', ' ']))
    {
        return Err(io::Error::other(format!("artifact path is not portable: {name:?}")));
    }
    if name.len() > MAX_PATH_BYTES || name.split('/').count() > MAX_COMPONENTS {
        return Err(io::Error::other("artifact path exceeds length or nesting limits"));
    }
    Ok(())
}

pub(super) fn collect(directory: &Path, prefix: &str, files: &mut Files) -> io::Result<()> {
    if prefix.split('/').count() > MAX_COMPONENTS - 1 {
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

pub(super) fn digest<'a>(files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> String {
    let mut digest = Sha256::new();
    for (name, bytes) in files {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    format!("{:x}", digest.finalize())
}

#[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
pub(super) fn rename_directory(source: &Path, destination: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, source, CWD, destination, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
pub(super) fn rename_directory(source: &Path, destination: &Path) -> io::Result<()> {
    // Windows directory rename fails if the destination already exists.
    fs::rename(source, destination)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_vendor = "apple", windows)))]
pub(super) fn rename_directory(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "atomic directory publication is unsupported on this OS"))
}
