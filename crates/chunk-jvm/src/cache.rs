//! Verified release installs under `<cache>/releases/<release_id>`, AOT caches under
//! `<cache>/aot/<release_id>/<app>.aot`, and asset blobs in the store at `<cache>/assets`, which may outlive the
//! machine. An AOT cache is kept outside its release, whose verification hashes every file there.

use crate::Failure;
use chunk_build::{Installed, VerifiedRelease, assets::Store, install_trusted_release, installed_release};
use chunk_proto::sync::v1::JvmLaunch;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Instant,
};
use tempfile::NamedTempFile;

pub(crate) struct Cache {
    releases: PathBuf,
    aot: PathBuf,
    assets: Store,
}

impl Cache {
    /// Opens the cache at `root`, removing the hidden staging files and directories earlier runs left behind.
    pub fn open(root: &Path) -> Result<Self, Failure> {
        let (releases, aot, assets) = (root.join("releases"), root.join("aot"), root.join("assets"));
        let io =
            |error: io::Error| Failure::io(format!("cannot open the release cache at {}: {error}", root.display()));
        for directory in [&releases, &aot, &assets, &assets.join("blobs")] {
            fs::create_dir_all(directory).map_err(io)?;
            for entry in fs::read_dir(directory).map_err(io)? {
                let path = entry.map_err(io)?.path();
                if path.file_name().is_some_and(|name| name.to_string_lossy().starts_with('.')) {
                    remove(&path).map_err(io)?;
                }
            }
        }
        Ok(Self { releases, aot, assets: Store::new(assets) })
    }

    /// Where release `id` is installed.
    pub fn directory(&self, id: &str) -> Result<PathBuf, Failure> {
        if !plain(id) {
            return Err(Failure::verify(format!("core named an invalid release ID {id:?}")));
        }
        Ok(self.releases.join(id))
    }

    /// Where the AOT cache of `launch`'s release and app is kept.
    pub fn aot(&self, launch: &JvmLaunch) -> Result<PathBuf, Failure> {
        if !plain(&launch.release_id) || !plain(&launch.app) {
            return Err(Failure::verify("core named an invalid release or app ID"));
        }
        Ok(self.aot.join(&launch.release_id).join(format!("{}.aot", launch.app)))
    }

    /// A hidden file to download an archive into, removed once dropped.
    pub fn staging(&self) -> Result<NamedTempFile, Failure> {
        staging(&self.releases, ".archive-")
    }

    /// A hidden file to download an AOT cache into, removed once dropped.
    pub fn aot_staging(&self) -> Result<NamedTempFile, Failure> {
        staging(&self.aot, ".aot-")
    }

    /// The store of the asset blobs this machine fetched.
    pub fn assets(&self) -> &Store {
        &self.assets
    }

    /// A hidden file to download an asset blob into, removed once dropped.
    pub fn blob_staging(&self) -> Result<NamedTempFile, Failure> {
        staging(self.assets.root(), ".blob-")
    }
}

fn staging(directory: &Path, prefix: &str) -> Result<NamedTempFile, Failure> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempfile_in(directory)
        .map_err(|error| Failure::io(format!("cannot stage a download: {error}")))
}

fn plain(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && !id.starts_with('.')
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

/// The install of release `id` at `directory` if it verifies as that release, which an install a crash cut short does
/// not. Anything else there is removed.
pub(crate) fn cached(directory: &Path, id: &str) -> Result<Option<VerifiedRelease>, Failure> {
    let io = |error: io::Error| Failure::io(format!("cannot check the cached release: {error}"));
    let checking = Instant::now();
    match installed_release(directory, id).map_err(io)? {
        Installed::Verified(release) => {
            tracing::info!(release = id, elapsed = ?checking.elapsed(), "reusing the cached release");
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

/// Unpacks the archive staged in `staging`, which matched `launch`'s digest as it streamed, and installs it at
/// `directory` as `launch`'s release. Core verified that release before naming it, so it isn't verified again.
pub(crate) async fn install(
    staging: NamedTempFile,
    launch: &JvmLaunch,
    directory: &Path,
) -> Result<VerifiedRelease, Failure> {
    let (id, directory) = (launch.release_id.clone(), directory.to_owned());
    tokio::task::spawn_blocking(move || install_trusted_release(staging.path(), &id, &directory))
        .await
        .map_err(Failure::io)?
        .map_err(|error| Failure::verify(format!("cannot install the release archive: {error}")))
}

fn remove(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) }
}
