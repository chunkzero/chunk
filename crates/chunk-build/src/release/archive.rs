use std::{
    fs,
    io::{self, Read},
    path::Path,
};

use tempfile::NamedTempFile;

use crate::publication::{Files, exists};

pub(super) fn prepare(directory: &Path, files: &Files) -> io::Result<NamedTempFile> {
    fs::create_dir_all(directory)?;
    let mut temporary = NamedTempFile::new_in(directory)?;
    let gzip = flate2::GzBuilder::new()
        .mtime(0)
        .operating_system(255)
        .write(temporary.as_file_mut(), flate2::Compression::default());
    let mut archive = tar::Builder::new(gzip);
    for (name, bytes) in files {
        let mut header = tar::Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_username("")?;
        header.set_groupname("")?;
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        archive.append_data(&mut header, name, bytes.as_slice())?;
    }
    archive.into_inner()?.finish()?;
    temporary.as_file().sync_all()?;
    Ok(temporary)
}

pub(super) fn verify_existing(temporary: &NamedTempFile, destination: &Path) -> io::Result<()> {
    if exists(destination)? {
        if !fs::symlink_metadata(destination)?.is_file() {
            return Err(io::Error::other("published archive requires a regular file"));
        }
        let mut expected = fs::File::open(temporary.path())?;
        let mut actual = fs::File::open(destination)?;
        if expected.metadata()?.len() != actual.metadata()?.len() {
            return Err(io::Error::other("published archive was modified"));
        }
        let mut left = vec![0; 65_536];
        let mut right = vec![0; 65_536];
        loop {
            let count = expected.read(&mut left)?;
            if count == 0 {
                break;
            }
            actual.read_exact(&mut right[..count])?;
            if left[..count] != right[..count] {
                return Err(io::Error::other("published archive was modified"));
            }
        }
    }
    Ok(())
}

pub(super) fn publish(temporary: NamedTempFile, destination: &Path) -> io::Result<()> {
    let mut permissions = temporary.as_file().metadata()?.permissions();
    permissions.set_readonly(true);
    temporary.as_file().set_permissions(permissions)?;
    match temporary.persist_noclobber(destination) {
        Ok(_) => Ok(()),
        Err(error) if exists(destination)? => verify_existing(&error.file, destination),
        Err(error) => Err(error.error),
    }
}
