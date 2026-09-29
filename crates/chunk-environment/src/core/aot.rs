//! The Leyden AOT caches remote runners make and use. Each belongs to a release, an app and the Java runtime a runner
//! reports, and lives at `<state>/aot/<release>/<app>/<runtime key>`, the key being the SHA-256 of that runtime. One host
//! at a time records the cache a key lacks, and its release waits for its upload.

use chunk_proto::sync::v1::{Error, JvmAotRecord, JvmAotUse, JvmAotWrite, error::Code, jvm_launch::Aot};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, File},
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    time::Duration,
};
use tokio::sync::{OwnedMutexGuard, watch};

/// The largest cache core accepts.
const MAX_BYTES: u64 = 512 * 1024 * 1024;
/// The longest runtime a runner may report.
const RUNTIME_BYTES: usize = 256;
/// How long a recording host's release waits for its upload to end.
pub(crate) const UPLOAD_GRACE: Duration = Duration::from_secs(90);

pub(crate) struct AotCaches {
    root: PathBuf,
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// The size and digest of each installed cache core has hashed, by path.
    digests: BTreeMap<PathBuf, Cache>,
    /// What each host that launched does about its cache.
    hosts: BTreeMap<String, Host>,
}

/// An installed cache's size and SHA-256 digest in lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Cache {
    pub path: PathBuf,
    pub size: u64,
    pub sha256: String,
}

struct Host {
    boot: String,
    /// Where the cache of the host's release, app and runtime is kept.
    path: PathBuf,
    role: Role,
}

enum Role {
    Use(Cache),
    Record(Recording),
    /// Its recording ended; it never records again.
    Recorded,
}

struct Recording {
    upload: Arc<tokio::sync::Mutex<Option<Upload>>>,
    /// Dropped, which wakes every subscriber, once the recording ends.
    ended: watch::Sender<()>,
}

/// A cache being uploaded into a hidden file under the root, removed once dropped unless it was installed.
struct Upload {
    file: File,
    temp: PathBuf,
    size: u64,
    sha256: String,
    digest: Sha256,
    written: u64,
    /// Where the last chunk started.
    last: u64,
}

impl Drop for Upload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp);
    }
}

impl AotCaches {
    pub fn new(root: PathBuf) -> Self {
        Self { root, state: Arc::default() }
    }

    /// What `host`, booted as `boot`, does about the cache of `release`'s `app` for `runtime`: use it once it exists,
    /// record it when no other host does, or nothing. A repeat for a recording host tells it to record again.
    pub async fn plan(&self, host: &str, boot: &str, release: &str, app: &str, runtime: &str) -> Option<Aot> {
        if runtime.is_empty() || runtime.len() > RUNTIME_BYTES || !plain(release) || !plain(app) {
            return None;
        }
        let path = self.root.join(release).join(app).join(hex(&Sha256::digest(runtime)));
        let cache = self.installed(&path).await;
        let mut state = lock(&self.state);
        let recorded = match state.hosts.get(host) {
            Some(entry) if entry.boot != boot || entry.path != path => return None,
            Some(Host { role: Role::Record(_), .. }) => return Some(Aot::Record(JvmAotRecord {})),
            Some(Host { role: Role::Recorded, .. }) => true,
            _ => false,
        };
        let (boot, host) = (boot.to_owned(), host.to_owned());
        if let Some(cache) = cache {
            let plan = Aot::Use(JvmAotUse { size: cache.size, sha256: cache.sha256.clone() });
            state.hosts.insert(host, Host { boot, path, role: Role::Use(cache) });
            return Some(plan);
        }
        let recording = |entry: &Host| entry.path == path && matches!(entry.role, Role::Record(_));
        if recorded || state.hosts.values().any(recording) {
            return None;
        }
        tracing::info!(host, "no AOT cache yet; the host records it");
        let recording = Recording { upload: Arc::default(), ended: watch::channel(()).0 };
        state.hosts.insert(host, Host { boot, path, role: Role::Record(recording) });
        Some(Aot::Record(JvmAotRecord {}))
    }

    /// The cache `host`, booted as `boot`, was told to use.
    pub fn used(&self, host: &str, boot: &str) -> Option<Cache> {
        match lock(&self.state).hosts.get(host)? {
            Host { boot: bound, role: Role::Use(cache), .. } if bound == boot => Some(cache.clone()),
            _ => None,
        }
    }

    /// Takes one chunk of the cache `host`, booted as `boot`, records, and installs the cache once its last chunk
    /// matches the declared size and digest. A chunk that doesn't fit ends the recording.
    pub async fn write(&self, host: &str, boot: &str, write: JvmAotWrite) -> Result<(), Error> {
        let (upload, path) = match lock(&self.state).hosts.get(host) {
            Some(Host { boot: bound, path, role: Role::Record(recording) }) if bound == boot => {
                (recording.upload.clone(), path.clone())
            }
            _ => return Err(error(Code::Denied, "the host records no AOT cache, or its recording ended")),
        };
        if write.abandon {
            tracing::info!(host, "the runner made no AOT cache");
            end(&self.state, host, &upload);
            return Ok(());
        }
        let Ok(guard) = upload.clone().try_lock_owned() else {
            return Err(error(Code::Overloaded, "another AOT cache chunk of this host is being written"));
        };
        let (state, root, host) = (self.state.clone(), self.root.clone(), host.to_owned());
        let written = tokio::task::spawn_blocking(move || {
            let mut guard = guard;
            let appended = append(&mut guard, &root, &write).map_err(|error| error.to_string());
            match appended {
                Ok(false) => Ok(()),
                Ok(true) => install(&state, &host, &upload, guard, &path),
                Err(message) => {
                    tracing::warn!(host, message, "AOT cache upload failed");
                    end(&state, &host, &upload);
                    Err(error(Code::Invalid, message))
                }
            }
        });
        written.await.unwrap_or_else(|_| Err(error(Code::Unavailable, "writing the AOT cache failed")))
    }

    /// Waits until `host`'s recording, if it has one, ends, for up to `bound`, then ends it.
    pub async fn settle(&self, host: &str, bound: Duration) {
        let mut ended = match lock(&self.state).hosts.get(host) {
            Some(Host { role: Role::Record(recording), .. }) => recording.ended.subscribe(),
            _ => return,
        };
        tracing::info!(host, "waiting for the host's AOT cache upload before releasing it");
        // Only the sender dropping, as the recording ends, wakes this.
        if tokio::time::timeout(bound, ended.changed()).await.is_err() {
            tracing::warn!(host, ?bound, "the host's AOT cache upload did not end in time; releasing it");
            if let Some(entry) = lock(&self.state).hosts.get_mut(host)
                && matches!(entry.role, Role::Record(_))
            {
                entry.role = Role::Recorded;
            }
        }
    }

    /// Forgets `host`, released for good.
    pub fn forget(&self, host: &str) {
        lock(&self.state).hosts.remove(host);
    }

    /// The cache at `path`, hashed once while core runs.
    async fn installed(&self, path: &Path) -> Option<Cache> {
        let size = fs::metadata(path).ok().filter(fs::Metadata::is_file)?.len();
        if let Some(cache) = lock(&self.state).digests.get(path).filter(|cache| cache.size == size) {
            return Some(cache.clone());
        }
        let file = path.to_owned();
        let hashed = tokio::task::spawn_blocking(move || {
            let mut digest = Sha256::new();
            io::copy(&mut File::open(&file)?, &mut digest)?;
            Ok::<_, io::Error>(hex(&digest.finalize()))
        });
        let cache = Cache { path: path.to_owned(), size, sha256: hashed.await.ok()?.ok()? };
        lock(&self.state).digests.insert(path.to_owned(), cache.clone());
        Some(cache)
    }
}

/// Appends `write`'s chunk to the upload, starting it with the first, and returns whether the cache is complete. A
/// repeat of the last chunk changes nothing.
fn append(upload: &mut Option<Upload>, root: &Path, write: &JvmAotWrite) -> io::Result<bool> {
    let invalid = |message: &str| Err(io::Error::other(message));
    let length = write.data.len() as u64;
    if !(1..=MAX_BYTES).contains(&write.size) || !sha256(&write.sha256) || length == 0 {
        return invalid("an AOT cache chunk carries bytes of a cache of 1 byte to 512 MiB and its SHA-256");
    }
    if upload.is_none() {
        if write.offset != 0 {
            return invalid("the first AOT cache chunk starts at offset 0");
        }
        fs::create_dir_all(root)?;
        let temp = root.join(format!(".upload-{}", uuid::Uuid::new_v4()));
        let file = File::create_new(&temp)?;
        let (size, sha256) = (write.size, write.sha256.clone());
        *upload = Some(Upload { file, temp, size, sha256, digest: Sha256::new(), written: 0, last: 0 });
    }
    let Some(upload) = upload else { return invalid("no upload") };
    if upload.size != write.size || upload.sha256 != write.sha256 {
        return invalid("the AOT cache's size or digest changed");
    }
    if upload.written > 0 && write.offset == upload.last && write.offset + length == upload.written {
        return Ok(false);
    }
    if write.offset != upload.written || upload.written + length > upload.size {
        return invalid("the AOT cache chunk is out of order or past the declared size");
    }
    upload.file.write_all(&write.data)?;
    upload.digest.update(&write.data);
    (upload.last, upload.written) = (write.offset, upload.written + length);
    if upload.written < upload.size {
        return Ok(false);
    }
    upload.file.sync_all()?;
    if hex(&upload.digest.clone().finalize()) != upload.sha256 {
        return invalid("the AOT cache differs from its declared SHA-256");
    }
    Ok(true)
}

/// Moves the complete upload into place at `path`, unless `host`'s recording `upload` ended meanwhile, and ends it.
fn install(
    state: &Mutex<State>,
    host: &str,
    upload: &Arc<tokio::sync::Mutex<Option<Upload>>>,
    mut guard: OwnedMutexGuard<Option<Upload>>,
    path: &Path,
) -> Result<(), Error> {
    let finished = guard.take();
    let mut state = lock(state);
    if !state.hosts.get(host).is_some_and(|entry| recording(entry, upload)) {
        return Err(error(Code::Denied, "the host's AOT cache recording ended"));
    }
    // The cache comes from this environment's own JVM, and only this environment's JVMs of the same release and app
    // use it, so a tenant can at worst spoil its own JVMs' startup. Java rejects a cache that doesn't fit its runtime.
    let installed = finished.ok_or_else(|| io::Error::other("the upload is empty")).and_then(|finished| {
        path.parent().map_or(Ok(()), fs::create_dir_all)?;
        fs::rename(&finished.temp, path)?;
        Ok(Cache { path: path.to_owned(), size: finished.size, sha256: finished.sha256.clone() })
    });
    if let Some(entry) = state.hosts.get_mut(host) {
        entry.role = Role::Recorded;
    }
    match installed {
        Ok(cache) => {
            tracing::info!(host, size = cache.size, "installed the AOT cache");
            state.digests.insert(path.to_owned(), cache);
            Ok(())
        }
        Err(failure) => {
            tracing::warn!(host, %failure, "the AOT cache was not installed");
            Err(error(Code::Unavailable, format!("installing the AOT cache failed: {failure}")))
        }
    }
}

/// Ends `host`'s recording, if `upload` is still its upload.
fn end(state: &Mutex<State>, host: &str, upload: &Arc<tokio::sync::Mutex<Option<Upload>>>) {
    if let Some(entry) = lock(state).hosts.get_mut(host).filter(|entry| recording(entry, upload)) {
        entry.role = Role::Recorded;
    }
}

fn recording(entry: &Host, upload: &Arc<tokio::sync::Mutex<Option<Upload>>>) -> bool {
    matches!(&entry.role, Role::Record(recording) if Arc::ptr_eq(&recording.upload, upload))
}

/// A release or app ID that is a safe path component.
fn plain(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && !id.starts_with('.')
        && id.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
}

fn sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
}

fn error(code: Code, message: impl Into<String>) -> Error {
    Error { code: code.into(), message: message.into() }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}
