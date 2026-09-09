use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use sha2::{Digest, Sha256};

/// Keeps child launches on one executable even when a build replaces the original binary.
/// # Errors
/// Rejects an oversized binary, modified pinned copy, or failed atomic publication.
pub fn pin_program(program: &Path, directory: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    let digest = copy_and_hash(program, temporary.as_file_mut())?;
    temporary.as_file().sync_all()?;
    let mut permissions = fs::metadata(program)?.permissions();
    permissions.set_readonly(true);
    temporary.as_file().set_permissions(permissions)?;
    let destination = directory.join(format!("{digest}{}", std::env::consts::EXE_SUFFIX));
    match temporary.persist_noclobber(&destination) {
        Ok(file) => drop(file),
        Err(error) if destination.exists() => drop(error),
        Err(error) => return Err(error.error),
    }
    if copy_and_hash(&destination, &mut io::sink())? != digest {
        return Err(io::Error::other("pinned platform executable was modified"));
    }
    destination.canonicalize()
}

fn copy_and_hash(path: &Path, output: &mut impl Write) -> io::Result<String> {
    if !fs::symlink_metadata(path)?.file_type().is_file() {
        return Err(io::Error::other("platform executable requires a regular file"));
    }
    let mut source = fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0; 65_536];
    let mut total = 0;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count;
        if total > 1024 * 1024 * 1024 {
            return Err(io::Error::other("platform executable exceeds local limit"));
        }
        digest.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    Ok(format!("{:x}", digest.finalize()))
}
