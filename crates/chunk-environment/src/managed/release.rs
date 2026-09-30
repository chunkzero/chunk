//! Loading the releases the management service deploys, checked with `chunk build`'s own rules.

use crate::core::{Archives, ReleaseArchive};
use chunk_build::{ArchiveDigest, Installed, VerifiedRelease};
use chunk_management::{Client, v1};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const DOWNLOAD_ATTEMPTS: u32 = 3;
/// A download attempt that receives nothing for this long fails.
const STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// A download attempt may take this long, plus a second for every `MIN_BYTES_PER_SECOND` of the archive.
const DOWNLOAD_BUDGET: Duration = Duration::from_secs(60);
const MIN_BYTES_PER_SECOND: u64 = 256 * 1024;
/// JVMs one release may run at once: control's limit, since capacity bounds them.
const MAX_PROCESSES: u16 = 32;

/// Where loaded releases live: each unpacked at `releases/<id>`, beside the archive it was installed from at
/// `archives/<id>.tar.gz` and the digest that archive was verified against at `archives/<id>.json`. Core's archive
/// lookup names the kept archives. Remote runners' AOT caches for a release live under `aot/<id>`, and go with it.
pub(super) struct Store {
    releases: PathBuf,
    archives: PathBuf,
    aot: PathBuf,
    claims: Claims,
    kept: Arc<Archives>,
}

impl Store {
    pub(super) fn new(state: &Path, kept: Arc<Archives>) -> Self {
        Self {
            releases: state.join("releases"),
            archives: state.join("archives"),
            aot: state.join("aot"),
            claims: Claims::default(),
            kept,
        }
    }
}

/// The digest a kept archive was verified against.
#[derive(Serialize, Deserialize)]
struct Checked {
    sha256: String,
    size: u64,
}

/// A verified release, unpacked under the state directory, which reclamation leaves alone until it is dropped.
pub(super) struct Loaded {
    release: VerifiedRelease,
    _claim: Claim,
}

/// The releases a load, its filesystem workers or a loaded release use. Each release has one user at a time, and
/// reclamation leaves them alone.
#[derive(Default)]
struct Claims(Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>);

/// The exclusive use of one release's directory and archive, until dropped.
struct Claim {
    _release: tokio::sync::OwnedMutexGuard<()>,
}

impl Claims {
    async fn claim(&self, id: &str) -> Claim {
        let release = self.0.lock().unwrap_or_else(PoisonError::into_inner).entry(id.into()).or_default().clone();
        Claim { _release: release.lock_owned().await }
    }

    /// The releases claimed or waited for.
    fn used(&self) -> BTreeSet<String> {
        let mut claims = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        claims.retain(|_, release| Arc::strong_count(release) > 1);
        claims.keys().cloned().collect()
    }
}

/// A file only one load attempt uses, removed once dropped.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        _ = fs::remove_file(&self.0);
    }
}

/// Installs the release `artifact` names at `releases/<release_id>` and keeps its verified archive. An earlier install
/// is reused while it verifies, or installed again from its kept archive; a missing or differing archive is downloaded
/// again, leaving the install as it is until the new archive verifies. Returns `None` once `cancel` stops it before its
/// download finishes.
pub(super) async fn load(
    client: &Client,
    store: &Store,
    artifact: &v1::ReleaseArtifact,
    cancel: &CancellationToken,
) -> io::Result<Option<Loaded>> {
    let id = artifact.release_id.clone();
    if !plain(&id) {
        return Err(io::Error::other("the release ID is not a plain name"));
    }
    fs::create_dir_all(&store.releases)?;
    fs::create_dir_all(&store.archives)?;
    let claim = tokio::select! {
        claim = store.claims.claim(&id) => claim,
        () = cancel.cancelled() => return Ok(None),
    };
    // Each blocking worker holds the claim until it finishes, even once this future is dropped.
    let directory = store.releases.join(&id);
    let digest = ArchiveDigest { sha256: artifact.sha256.clone(), size: artifact.size_bytes };
    let (existing, archives, kept) = (directory.clone(), store.archives.clone(), store.kept.clone());
    let (expected, release) = (digest.clone(), id.clone());
    let (reused, claim) =
        blocking(move || Ok((reusable(&existing, &archives, &expected, &release, &kept)?, claim))).await?;
    let (release, claim) = if let Some(release) = reused {
        (release, claim)
    } else {
        let staged = Staged(store.archives.join(format!(".{}.tar.gz", uuid::Uuid::new_v4())));
        tokio::select! {
            downloaded = download(client, artifact, &staged.0) => downloaded?,
            () = cancel.cancelled() => return Ok(None),
        }
        let (destination, archives, expected) = (directory.clone(), store.archives.clone(), digest.clone());
        blocking(move || {
            let release = chunk_build::install_release(&staged.0, &expected, &id, &destination)?;
            fs::File::open(&staged.0)?.sync_all()?;
            fs::rename(&staged.0, archive_path(&archives, &id))?;
            record(&archives, &id, &expected)?;
            Ok((release, claim))
        })
        .await?
    };
    let path = archive_path(&store.archives, &artifact.release_id);
    store.kept.insert(artifact.release_id.clone(), ReleaseArchive { path, sha256: digest.sha256, size: digest.size });
    Ok(Some(Loaded { release, _claim: claim }))
}

/// The install of release `id` at `directory`, installed again from its archive kept in `archives` unless it still
/// verifies. When that archive is missing or differs from `digest`, `kept` forgets it and it is removed, while the
/// install stays as it is. The caller's claim means no other load uses them.
fn reusable(
    directory: &Path,
    archives: &Path,
    digest: &ArchiveDigest,
    id: &str,
    kept: &Archives,
) -> io::Result<Option<VerifiedRelease>> {
    let archive = archive_path(archives, id);
    if !intact(&archive, digest)? {
        kept.remove(id);
        remove_path(&archive)?;
        remove_path(&record_path(archives, id))?;
        return Ok(None);
    }
    record(archives, id, digest)?;
    match chunk_build::installed_release(directory, id)? {
        Installed::Verified(release) => return Ok(Some(*release)),
        Installed::Invalid(error) => {
            tracing::warn!(%error, release = id, "unpacked release fails verification; installing it again");
        }
        Installed::Missing => {}
    }
    chunk_build::install_release(&archive, digest, id, directory).map(Some)
}

/// Whether the archive of `artifact`'s release is kept with the digest `artifact` names.
pub(super) fn kept(store: &Store, artifact: &v1::ReleaseArtifact) -> bool {
    let kept = store.kept.get(&artifact.release_id);
    kept.is_some_and(|kept| kept.sha256 == artifact.sha256 && kept.size == artifact.size_bytes)
}

/// Restores core's lookup of the archives kept for the `retained` releases, each only while its file still matches the
/// digest recorded when it was verified.
pub(super) async fn restore(store: &Store, retained: BTreeSet<String>) -> io::Result<()> {
    let (archives, kept) = (store.archives.clone(), store.kept.clone());
    blocking(move || {
        for id in retained.into_iter().filter(|id| plain(id)) {
            match restorable(&archives, &id) {
                Ok(Some(archive)) => kept.insert(id, archive),
                Ok(None) => {}
                Err(error) => tracing::warn!(%error, release = id, "kept release archive not restored"),
            }
        }
        Ok(())
    })
    .await
}

fn restorable(archives: &Path, id: &str) -> io::Result<Option<ReleaseArchive>> {
    let checked: Checked = match fs::read(record_path(archives, id)) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let digest = ArchiveDigest { sha256: checked.sha256, size: checked.size };
    let path = archive_path(archives, id);
    if !intact(&path, &digest)? {
        return Err(io::Error::other("the archive differs from the digest it was verified against"));
    }
    Ok(Some(ReleaseArchive { path, sha256: digest.sha256, size: digest.size }))
}

fn intact(archive: &Path, digest: &ArchiveDigest) -> io::Result<bool> {
    match fs::File::open(archive) {
        Ok(file) => digest.matches(file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Durably records `digest` as the one release `id`'s kept archive was verified against, then syncs `archives` so
/// the renamed archive and record both survive a crash.
fn record(archives: &Path, id: &str, digest: &ArchiveDigest) -> io::Result<()> {
    let checked = Checked { sha256: digest.sha256.clone(), size: digest.size };
    let staged = Staged(archives.join(format!(".{}.json", uuid::Uuid::new_v4())));
    let mut file = fs::File::create(&staged.0)?;
    file.write_all(&serde_json::to_vec(&checked).map_err(io::Error::other)?)?;
    file.sync_all()?;
    fs::rename(&staged.0, record_path(archives, id))?;
    fs::File::open(archives)?.sync_all()
}

fn archive_path(archives: &Path, id: &str) -> PathBuf {
    archives.join(format!("{id}.tar.gz"))
}

fn record_path(archives: &Path, id: &str) -> PathBuf {
    archives.join(format!("{id}.json"))
}

fn plain(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// Removes the downloads, installs and AOT cache uploads a previous run left unfinished: every entry named with a dot.
pub(super) async fn sweep(store: &Store) -> io::Result<()> {
    let mut unfinished = entries(&store.releases)?;
    unfinished.extend(entries(&store.archives)?);
    unfinished.extend(entries(&store.aot)?);
    remove(unfinished.into_iter().filter(|path| hidden(path)).collect()).await
}

/// Renames aside the installs, archives, digest records and AOT caches of the releases neither in `used` nor claimed,
/// returning their new paths. Core's archive lookup forgets them first.
pub(super) fn set_aside(store: &Store, mut used: BTreeSet<String>) -> Vec<PathBuf> {
    used.extend(store.claims.used());
    store.kept.retain(|release| used.contains(release));
    let mut paths = entries(&store.releases).unwrap_or_default();
    paths.extend(entries(&store.archives).unwrap_or_default());
    paths.extend(entries(&store.aot).unwrap_or_default());
    let unused = paths.into_iter().filter(|path| {
        let name = path.file_name().and_then(|name| name.to_str());
        let release = name.map(|name| name.strip_suffix(".tar.gz").or(name.strip_suffix(".json")).unwrap_or(name));
        !hidden(path) && release.is_some_and(|release| !used.contains(release))
    });
    unused
        .filter_map(|path| {
            let aside = path.with_file_name(format!(".unused-{}", uuid::Uuid::new_v4()));
            fs::rename(&path, &aside).inspect_err(|error| tracing::warn!(%error, ?path, "unused release kept")).ok()?;
            Some(aside)
        })
        .collect()
}

/// Removes `paths`, files or directories.
pub(super) async fn remove(paths: Vec<PathBuf>) -> io::Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    blocking(move || paths.iter().try_for_each(|path| remove_path(path))).await
}

fn remove_path(path: &Path) -> io::Result<()> {
    let removed = if path.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    match removed {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

fn entries(directory: &Path) -> io::Result<Vec<PathBuf>> {
    match fs::read_dir(directory) {
        Ok(entries) => entries.map(|entry| entry.map(|entry| entry.path())).collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn hidden(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name.as_encoded_bytes().starts_with(b"."))
}

impl Loaded {
    /// The release's backend as the backend version `deployment`.
    pub(super) fn bundle(&self, deployment: &str) -> chunk_contract::Deployment {
        let mut bundle = self.release.backend.clone();
        deployment.clone_into(&mut bundle.id);
        bundle
    }

    /// The release as control runs it for `deployment`.
    pub(super) fn control(&self, environment: &str, deployment: &str) -> chunk_control::Release {
        let release = &self.release;
        let session_types = release
            .apps
            .iter()
            .flat_map(|app| {
                app.sessions.iter().map(|(id, session)| {
                    let session_type = chunk_control::SessionType {
                        app: app.id.clone(),
                        machine_profile: session.machine_profile.clone(),
                        capacity: session.capacity,
                    };
                    (format!("{}/{id}", app.id), session_type)
                })
            })
            .collect();
        let profiles = release.profiles.iter().map(|(name, profile)| {
            let profile =
                chunk_control::MachineProfile { memory_mib: profile.memory_mib, max_sessions: profile.max_sessions };
            (name.clone(), profile)
        });
        let contracts = &release.backend.contracts;
        chunk_control::Release {
            apps: release.apps.iter().map(|app| (app.id.clone(), app.clone())).collect(),
            deployment: chunk_proto::control::v1::DeploymentRef {
                environment: environment.into(),
                deployment: deployment.into(),
            },
            release_id: release.id.clone(),
            profiles: profiles.collect(),
            session_types,
            max_processes: MAX_PROCESSES,
            idle_node_timeout_seconds: chunk_control::DEFAULT_IDLE_NODE_TIMEOUT_SECONDS,
            contracts: chunk_control::Contracts {
                session_methods: contracts.session_methods.clone(),
                session_configurations: contracts.session_configurations.clone(),
                destinations: contracts.destinations.clone(),
            },
        }
    }
}

/// Downloads the archive to `path`, retrying failed or overdue transfers, and stops early once it outgrows its declared
/// size.
async fn download(client: &Client, artifact: &v1::ReleaseArtifact, path: &Path) -> io::Result<()> {
    let budget = DOWNLOAD_BUDGET + Duration::from_secs(artifact.size_bytes / MIN_BYTES_PER_SECOND);
    let mut attempt = 1;
    loop {
        let result = tokio::time::timeout(budget, download_once(client, artifact, path)).await.unwrap_or_else(|_| {
            let error = format!("it took longer than {}s", budget.as_secs());
            Err(Failure::Transfer(io::Error::new(io::ErrorKind::TimedOut, error)))
        });
        match result {
            Err(Failure::Transfer(error)) if attempt < DOWNLOAD_ATTEMPTS => {
                tracing::warn!(%error, release = artifact.release_id, "release download failed; retrying");
                tokio::time::sleep(Duration::from_secs(u64::from(attempt))).await;
                attempt += 1;
            }
            Err(Failure::Transfer(error)) => {
                return Err(io::Error::other(format!("release download failed: {error}")));
            }
            Err(Failure::Local(error)) => return Err(error),
            Ok(()) => return Ok(()),
        }
    }
}

enum Failure {
    Transfer(io::Error),
    Local(io::Error),
}

async fn download_once(client: &Client, artifact: &v1::ReleaseArtifact, path: &Path) -> Result<(), Failure> {
    let mut download = stalled(client.download_archive(&artifact.url)).await?;
    let mut file = tokio::fs::File::create(path).await.map_err(Failure::Local)?;
    let mut size = 0;
    while let Some(chunk) = stalled(download.chunk()).await? {
        size += chunk.len() as u64;
        if size > artifact.size_bytes {
            return Err(Failure::Local(io::Error::other("the release archive is larger than its declared size")));
        }
        file.write_all(&chunk).await.map_err(Failure::Local)?;
    }
    file.flush().await.map_err(Failure::Local)
}

/// `transfer`'s result, or a failed transfer once it has waited `STALL_TIMEOUT`.
async fn stalled<T>(transfer: impl Future<Output = Result<T, chunk_management::Error>>) -> Result<T, Failure> {
    match tokio::time::timeout(STALL_TIMEOUT, transfer).await {
        Ok(result) => result.map_err(|error| Failure::Transfer(io::Error::other(error))),
        Err(_) => Err(Failure::Transfer(io::Error::new(io::ErrorKind::TimedOut, "the transfer stalled"))),
    }
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(io::Error::other)?
}
