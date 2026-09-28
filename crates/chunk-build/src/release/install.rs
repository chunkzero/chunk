use std::{fs, io, path::Path};

use super::{ArchiveDigest, UnpackLimits, VerifiedRelease, unpack_release, verify_release};
use crate::publication;

/// What [`installed_release`] found at an install directory.
#[derive(Debug)]
pub enum Installed {
    Missing,
    Verified(Box<VerifiedRelease>),
    /// The install fails verification as the release for this reason.
    Invalid(io::Error),
}

/// Checks the install of release `id` at `directory`, leaving it as it is.
/// # Errors
/// Reports an install that cannot be inspected.
pub fn installed_release(directory: &Path, id: &str) -> io::Result<Installed> {
    let metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Installed::Missing),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() {
        return Ok(Installed::Invalid(io::Error::other("the install is not a directory")));
    }
    Ok(match verify(directory, id) {
        Ok(release) => Installed::Verified(Box::new(release)),
        Err(reason) => Installed::Invalid(reason),
    })
}

/// Unpacks `archive`, checked against `expected`, and verifies that it holds release `id`. Only then is it moved to
/// `directory`, unless an install there already verifies as that release, which is kept; an invalid install there is
/// replaced.
/// # Errors
/// Rejects everything [`unpack_release`] and [`verify_release`] reject and a release other than `id`, leaving
/// `directory` as it is.
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
    match installed_release(directory, id)? {
        Installed::Verified(existing) => return Ok(*existing),
        Installed::Invalid(_) => fs::rename(directory, staging.path().join("replaced"))?,
        Installed::Missing => {}
    }
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
