use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// Keeps child launches on one executable even when a build replaces the original binary.
/// # Errors
/// Rejects an oversized binary, modified pinned copy, or failed atomic publication.
pub fn pin_program(program: &Path, directory: &Path) -> io::Result<PathBuf> {
    let bytes = super::read_limited(program, 256 * 1024 * 1024)?;
    fs::create_dir_all(directory)?;
    let destination = directory.join(format!("{:x}{}", Sha256::digest(&bytes), std::env::consts::EXE_SUFFIX));
    if !destination.exists() {
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        temporary.write_all(&bytes)?;
        temporary.as_file().sync_all()?;
        let mut permissions = fs::metadata(program)?.permissions();
        permissions.set_readonly(true);
        temporary.as_file().set_permissions(permissions)?;
        match temporary.persist_noclobber(&destination) {
            Ok(file) => drop(file),
            Err(error) if destination.exists() => drop(error),
            Err(error) => return Err(error.error),
        }
    }
    if super::read_limited(&destination, 256 * 1024 * 1024)? != bytes {
        return Err(io::Error::other("pinned platform executable was modified"));
    }
    destination.canonicalize()
}
