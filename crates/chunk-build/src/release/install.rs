use std::{fs, io, path::Path};

use super::{ArchiveDigest, UnpackLimits, VerifiedRelease, unpack_release, verify_release};
use crate::publication;

/// What [`installed_release`] found at an install directory.
#[derive(Debug)]
pub enum Installed {
    Missing,
    Verified(Box<VerifiedRelease>),
    /// The install failed verification for this reason, and was removed.
    Removed(io::Error),
}

/// Checks the install of release `id` at `directory`, removing it unless it still verifies as that release.
/// # Errors
/// Reports an install that cannot be inspected or removed.
pub fn installed_release(directory: &Path, id: &str) -> io::Result<Installed> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Installed::Missing),
        Err(error) => return Err(error),
    };
    let verified =
        if metadata.is_dir() { verify(directory, id) } else { Err(io::Error::other("the install is not a directory")) };
    match verified {
        Ok(release) => Ok(Installed::Verified(Box::new(release))),
        Err(reason) => {
            if metadata.is_dir() { fs::remove_dir_all(directory) } else { fs::remove_file(directory) }?;
            Ok(Installed::Removed(reason))
        }
    }
}

/// Unpacks `archive`, checked against `expected`, verifies that it holds release `id`, and moves it to the new
/// directory `directory`. Nothing is created at `directory` unless the release verifies.
/// # Errors
/// Rejects everything [`unpack_release`] and [`verify_release`] reject, a release other than `id` and an existing
/// `directory`.
pub fn install_release(
    archive: &Path,
    expected: &ArchiveDigest,
    id: &str,
    directory: &Path,
) -> io::Result<VerifiedRelease> {
    let parent = directory.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new().prefix(".install-").tempdir_in(parent)?;
    let unpacked = staging.path().join("release");
    unpack_release(archive, expected, &unpacked, &UnpackLimits::default())?;
    let release = verify(&unpacked, id)?;
    publication::rename_directory(&unpacked, directory)?;
    Ok(release)
}

fn verify(directory: &Path, id: &str) -> io::Result<VerifiedRelease> {
    let release = verify_release(directory)
        .map_err(|error| io::Error::other(format!("release {id} fails verification: {error}")))?;
    if release.id != id {
        return Err(io::Error::other(format!("the archive holds release {}, not {id}", release.id)));
    }
    Ok(release)
}
