//! Asset revisions on local disk: a content-addressed blob store, and the read-only directories JVMs read an app's
//! assets from.

use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use chunk_contract::AssetRevision;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;

#[cfg(feature = "compiler")]
mod build;
#[cfg(feature = "compiler")]
pub use build::build_revision;

/// A directory of blobs, `blobs/<sha256>`, and revisions, `revisions/<id>.json`. Writes are atomic and verified, and
/// nothing present is rewritten.
#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether the store holds the blob with this SHA-256.
    /// # Errors
    /// Rejects an invalid digest and blobs that cannot be inspected.
    pub fn contains(&self, sha256: &str) -> io::Result<bool> {
        match fs::symlink_metadata(self.blob(sha256)?) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Stores `bytes` as the blob with this SHA-256.
    /// # Errors
    /// Rejects bytes with another digest, and filesystem failures.
    pub fn insert(&self, sha256: &str, bytes: &[u8]) -> io::Result<()> {
        self.insert_from(sha256, bytes)
    }

    /// Copies the file at `path` into the store as the blob with this SHA-256.
    /// # Errors
    /// Rejects a file with another digest, and filesystem failures.
    pub fn insert_file(&self, sha256: &str, path: &Path) -> io::Result<()> {
        self.insert_from(sha256, fs::File::open(path)?)
    }

    fn insert_from(&self, sha256: &str, mut source: impl Read) -> io::Result<()> {
        let path = self.blob(sha256)?;
        if self.contains(sha256)? {
            return Ok(());
        }
        let mut temporary = temporary(&self.root.join("blobs"))?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            digest.update(&buffer[..read]);
            temporary.write_all(&buffer[..read])?;
        }
        if format!("{:x}", digest.finalize()) != sha256 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("asset blob {sha256} has another digest")));
        }
        persist(temporary, &path)
    }

    /// Writes the revision's canonical JSON, returning its ID.
    /// # Errors
    /// Rejects an invalid revision, a different file under its ID, and filesystem failures.
    pub fn write_revision(&self, revision: &AssetRevision) -> io::Result<String> {
        revision.validate().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let (id, bytes) = (revision.id(), revision.encode());
        let path = self.root.join("revisions").join(format!("{id}.json"));
        match fs::read(&path) {
            Ok(existing) if existing == bytes => return Ok(id),
            Ok(_) => return Err(io::Error::other(format!("{} was modified", path.display()))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let mut temporary = temporary(&self.root.join("revisions"))?;
        temporary.write_all(&bytes)?;
        persist(temporary, &path)?;
        Ok(id)
    }

    /// # Errors
    /// Rejects a missing revision, and one whose canonical JSON is invalid or has another ID.
    pub fn read_revision(&self, id: &str) -> io::Result<AssetRevision> {
        check_digest(id)?;
        let bytes = fs::read(self.root.join("revisions").join(format!("{id}.json")))?;
        let revision =
            AssetRevision::decode(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if revision.id() != id {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("asset revision {id} has another ID")));
        }
        Ok(revision)
    }

    fn blob(&self, sha256: &str) -> io::Result<PathBuf> {
        check_digest(sha256)?;
        Ok(self.root.join("blobs").join(sha256))
    }
}

/// Builds `app`'s read-only directory of `revision` at `<store>/apps/<revision ID>/<app>/`, or returns it as it is
/// when an earlier call built it:
///
/// - `revision.json`, the revision's canonical JSON;
/// - `worlds/<name>.polar`, the app's worlds;
/// - `app/<path>`, the app's files;
/// - `shared/<path>`, the project's shared files.
///
/// Files are hard links to the store's blobs, or read-only copies where linking fails. The directory appears
/// atomically and complete.
/// # Errors
/// Rejects an invalid revision or app ID, blobs missing from the store, and filesystem failures.
pub fn materialize(store: &Store, revision: &AssetRevision, app: &str) -> io::Result<PathBuf> {
    revision.validate().map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !crate::valid_id(app) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid app ID {app:?}")));
    }
    let parent = store.root.join("apps").join(revision.id());
    let destination = parent.join(app);
    if fs::symlink_metadata(&destination).is_ok() {
        return Ok(destination);
    }
    fs::create_dir_all(&parent)?;
    let staging = tempfile::Builder::new().prefix(".materialize-").tempdir_in(&parent)?;
    let mut revision_json = temporary(staging.path())?;
    revision_json.write_all(&revision.encode())?;
    persist(revision_json, &staging.path().join("revision.json"))?;
    let assets = revision.apps.get(app);
    let worlds = assets.into_iter().flat_map(|assets| &assets.worlds);
    let worlds = worlds.map(|(name, blob)| (format!("worlds/{name}.polar"), blob));
    let files = assets.into_iter().flat_map(|assets| &assets.files).map(|(path, blob)| (format!("app/{path}"), blob));
    let shared = revision.shared.iter().map(|(path, blob)| (format!("shared/{path}"), blob));
    for directory in ["worlds", "app", "shared"] {
        fs::create_dir(staging.path().join(directory))?;
    }
    for (path, blob) in worlds.chain(files).chain(shared) {
        let source = store.blob(&blob.sha256)?;
        if fs::symlink_metadata(&source).is_ok_and(|metadata| !metadata.is_file() || metadata.len() != blob.size) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("asset blob {} is corrupt", blob.sha256)));
        }
        let target = staging.path().join(path);
        target.parent().map_or(Ok(()), fs::create_dir_all)?;
        link(&source, &target).map_err(|error| {
            io::Error::new(error.kind(), format!("asset blob {} is unavailable: {error}", blob.sha256))
        })?;
    }
    match crate::publication::rename_directory(staging.path(), &destination) {
        Ok(()) => Ok(destination),
        Err(_) if fs::symlink_metadata(&destination).is_ok() => Ok(destination),
        Err(error) => Err(error),
    }
}

fn link(source: &Path, target: &Path) -> io::Result<()> {
    if fs::hard_link(source, target).is_ok() {
        return Ok(());
    }
    fs::copy(source, target)?;
    let mut permissions = fs::metadata(target)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(target, permissions)
}

fn temporary(directory: &Path) -> io::Result<NamedTempFile> {
    fs::create_dir_all(directory)?;
    tempfile::Builder::new().prefix(".write-").tempfile_in(directory)
}

/// Makes `temporary` read-only and durable at `path`, keeping a file another writer put there first.
fn persist(temporary: NamedTempFile, path: &Path) -> io::Result<()> {
    temporary.as_file().sync_all()?;
    let mut permissions = temporary.as_file().metadata()?.permissions();
    permissions.set_readonly(true);
    temporary.as_file().set_permissions(permissions)?;
    match temporary.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(_) if fs::symlink_metadata(path).is_ok() => Ok(()),
        Err(error) => Err(error.error),
    }
}

fn check_digest(value: &str) -> io::Result<()> {
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid SHA-256 {value:?}")));
    }
    Ok(())
}

#[cfg(all(test, feature = "compiler"))]
mod tests;
