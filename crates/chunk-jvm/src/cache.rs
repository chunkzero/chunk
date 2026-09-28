//! Verified release installs under `<cache>/releases/<release_id>`, which may outlive the machine.

use crate::Failure;
use chunk_build::{ArchiveDigest, Installed, VerifiedRelease, install_release, installed_release};
use chunk_proto::sync::v1::JvmLaunch;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use tempfile::NamedTempFile;

pub(crate) struct Cache {
    releases: PathBuf,
}

impl Cache {
    /// Opens the cache at `root`, removing the hidden staging files and directories earlier runs left behind.
    pub fn open(root: &Path) -> Result<Self, Failure> {
        let releases = root.join("releases");
        let io =
            |error: io::Error| Failure::io(format!("cannot open the release cache at {}: {error}", root.display()));
        fs::create_dir_all(&releases).map_err(io)?;
        for entry in fs::read_dir(&releases).map_err(io)? {
            let path = entry.map_err(io)?.path();
            if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
                remove(&path).map_err(io)?;
            }
        }
        Ok(Self { releases })
    }

    /// Where release `id` is installed.
    pub fn directory(&self, id: &str) -> Result<PathBuf, Failure> {
        let valid = (1..=128).contains(&id.len())
            && !id.starts_with('.')
            && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte));
        if !valid {
            return Err(Failure::verify(format!("core named an invalid release ID {id:?}")));
        }
        Ok(self.releases.join(id))
    }

    /// A hidden file to download an archive into, removed once dropped.
    pub fn staging(&self) -> Result<NamedTempFile, Failure> {
        tempfile::Builder::new()
            .prefix(".archive-")
            .tempfile_in(&self.releases)
            .map_err(|error| Failure::io(format!("cannot stage the archive: {error}")))
    }
}

/// The install of release `id` at `directory` if it verifies as that release. Anything else there is removed.
pub(crate) fn cached(directory: &Path, id: &str) -> Result<Option<VerifiedRelease>, Failure> {
    let io = |error: io::Error| Failure::io(format!("cannot check the cached release: {error}"));
    match installed_release(directory, id).map_err(io)? {
        Installed::Verified(release) => {
            tracing::info!(release = id, "reusing the cached release");
            Ok(Some(*release))
        }
        Installed::Missing => Ok(None),
        Installed::Invalid(reason) => {
            tracing::warn!(release = id, %reason, "removing a cached release that fails verification");
            remove(directory).map_err(io)?;
            Ok(None)
        }
    }
}

/// Unpacks the archive staged in `staging`, checked against `launch`'s digest, and installs it at `directory` once it
/// verifies as `launch`'s release.
pub(crate) async fn install(
    staging: NamedTempFile,
    launch: &JvmLaunch,
    directory: &Path,
) -> Result<VerifiedRelease, Failure> {
    let digest = ArchiveDigest { sha256: launch.archive_sha256.clone(), size: launch.archive_size };
    let (id, directory) = (launch.release_id.clone(), directory.to_owned());
    tokio::task::spawn_blocking(move || install_release(staging.path(), &digest, &id, &directory))
        .await
        .map_err(Failure::io)?
        .map_err(|error| Failure::verify(format!("the release archive fails verification: {error}")))
}

fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}
