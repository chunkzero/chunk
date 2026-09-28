use std::{
    fs,
    io::{self, BufReader, Read, Seek},
    path::Path,
};

use sha2::{Digest, Sha256};

use std::collections::BTreeSet;

use crate::publication::{self, MAX_BYTES, MAX_COMPONENTS, MAX_FILES, MAX_PATH_BYTES};

/// A GNU long name holds the path and a trailing NUL.
const LONG_NAME_LIMIT: u64 = MAX_PATH_BYTES as u64 + 1;

/// The size and lowercase hex SHA-256 a release archive must have.
#[derive(Clone, Debug)]
pub struct ArchiveDigest {
    pub sha256: String,
    pub size: u64,
}

impl ArchiveDigest {
    /// Whether `archive`, read to its end, has exactly this size and SHA-256.
    /// # Errors
    /// Reports read errors.
    pub fn matches(&self, archive: impl Read) -> io::Result<bool> {
        let mut digest = Sha256::new();
        let size = io::copy(&mut archive.take(self.size + 1), &mut digest)?;
        Ok(size == self.size && format!("{:x}", digest.finalize()) == self.sha256)
    }
}

/// How many filesystem entries, files and directories together, and content bytes an archive may unpack to.
/// The default admits every release `chunk build` can publish: each of its files may add a directory for every
/// other component of its path.
pub struct UnpackLimits {
    pub entries: usize,
    pub bytes: u64,
}

impl Default for UnpackLimits {
    fn default() -> Self {
        Self { entries: MAX_FILES * MAX_COMPONENTS, bytes: MAX_BYTES as u64 }
    }
}

/// Checks a release archive against `expected`, then extracts it into the new directory `destination`.
/// Nothing is created at `destination` unless every entry extracts.
/// # Errors
/// Rejects a size or digest mismatch, entries other than regular files, nonportable, escaping or deeper paths than
/// `chunk build` publishes, duplicate paths, archives beyond `limits` and an existing `destination`.
pub fn unpack_release(
    archive: &Path,
    expected: &ArchiveDigest,
    destination: &Path,
    limits: &UnpackLimits,
) -> io::Result<()> {
    let mut file = fs::File::open(archive)?;
    if !expected.matches(&file)? {
        return Err(io::Error::other("release archive differs from its expected size and SHA-256"));
    }
    file.rewind()?;
    let parent = destination.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new().prefix(".unpack-").tempdir_in(parent)?;
    extract(flate2::read::GzDecoder::new(BufReader::new(file)), staging.path(), limits)?;
    publication::rename_directory(staging.path(), destination)
}

fn extract(reader: impl Read, directory: &Path, limits: &UnpackLimits) -> io::Result<()> {
    let mut archive = tar::Archive::new(reader);
    let (mut entries, mut bytes, mut long_name) = (0, 0, None);
    let (mut files, mut directories) = (BTreeSet::new(), BTreeSet::new());
    for entry in archive.entries()?.raw(true) {
        let mut entry = entry?;
        let kind = entry.header().entry_type();
        if kind.is_gnu_longname() && long_name.is_none() {
            let mut name = Vec::new();
            (&mut entry).take(LONG_NAME_LIMIT + 1).read_to_end(&mut name)?;
            if name.len() as u64 > LONG_NAME_LIMIT {
                return Err(io::Error::other("release archive entry name exceeds limit"));
            }
            if name.last() == Some(&0) {
                name.pop();
            }
            long_name = Some(name);
            continue;
        }
        if !kind.is_file() {
            return Err(io::Error::other("release archive holds something other than regular files"));
        }
        let name = long_name.take().unwrap_or_else(|| entry.path_bytes().into_owned());
        let name = String::from_utf8(name).map_err(|_| io::Error::other("release archive paths must be UTF-8"))?;
        publication::relative_name(&name)?;
        let duplicate = || io::Error::other(format!("release archive holds {name:?} twice or as a directory"));
        if directories.contains(&name) || !files.insert(name.clone()) {
            return Err(duplicate());
        }
        entries += 1;
        let mut ancestor = name.as_str();
        // Deepest first: once one ancestor is known, so are all of its own.
        while let Some((parent, _)) = ancestor.rsplit_once('/')
            && !directories.contains(parent)
        {
            if files.contains(parent) {
                return Err(duplicate());
            }
            directories.insert(parent.to_owned());
            entries += 1;
            ancestor = parent;
        }
        bytes += entry.size();
        if entries > limits.entries || bytes > limits.bytes {
            return Err(io::Error::other("release archive exceeds unpack limits"));
        }
        let path = directory.join(&name);
        fs::create_dir_all(path.parent().ok_or_else(|| io::Error::other("release archive path"))?)?;
        io::copy(&mut entry, &mut fs::File::create_new(path)?)?;
    }
    if long_name.is_some() {
        return Err(io::Error::other("release archive ends inside an entry"));
    }
    Ok(())
}
