//! Loading the releases the management service deploys, checked with `chunk build`'s own rules.

use chunk_build::{ArchiveDigest, UnpackLimits, VerifiedRelease};
use chunk_management::{Client, v1};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
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

/// A verified release, unpacked under the state directory, which reclamation leaves alone until it is dropped.
pub(super) struct Loaded {
    directory: PathBuf,
    release: VerifiedRelease,
    _claim: Claim,
}

/// The releases a load, its filesystem workers or a loaded release use. Each release has one user at a time, and
/// reclamation leaves them alone.
#[derive(Default)]
pub(super) struct Claims(Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>);

/// The exclusive use of one release's directory, until dropped.
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

/// A path only one load attempt uses, removed once dropped.
struct Staged(PathBuf);

impl Drop for Staged {
    fn drop(&mut self) {
        _ = if self.0.is_dir() { fs::remove_dir_all(&self.0) } else { fs::remove_file(&self.0) };
    }
}

/// Unpacks and verifies the release `artifact` names into `releases/<release_id>`, reusing an earlier copy there that
/// still verifies. Returns `None` once `cancel` stops it before its download finishes.
pub(super) async fn load(
    client: &Client,
    releases: &Path,
    claims: &Claims,
    artifact: &v1::ReleaseArtifact,
    cancel: &CancellationToken,
) -> io::Result<Option<Loaded>> {
    let id = artifact.release_id.clone();
    let plain = !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !plain {
        return Err(io::Error::other("the release ID is not a plain name"));
    }
    fs::create_dir_all(releases)?;
    let claim = tokio::select! {
        claim = claims.claim(&id) => claim,
        () = cancel.cancelled() => return Ok(None),
    };
    // Each blocking worker holds the claim until it finishes, even once this future is dropped.
    let directory = releases.join(&id);
    let (existing, expected) = (directory.clone(), id.clone());
    let (reused, claim) = blocking(move || Ok((reusable(&existing, &expected), claim))).await?;
    if let Some(release) = reused {
        return Ok(Some(Loaded { directory, release, _claim: claim }));
    }
    let attempt = uuid::Uuid::new_v4();
    let archive = Staged(releases.join(format!(".{attempt}.tar.gz")));
    tokio::select! {
        downloaded = download(client, artifact, &archive.0) => downloaded?,
        () = cancel.cancelled() => return Ok(None),
    }
    let digest = ArchiveDigest { sha256: artifact.sha256.clone(), size: artifact.size_bytes };
    let (staging, destination) = (Staged(releases.join(format!(".{attempt}"))), directory.clone());
    let (release, claim) = blocking(move || {
        chunk_build::unpack_release(&archive.0, &digest, &staging.0, &UnpackLimits::default())?;
        drop(archive);
        let release = verify(&staging.0, &id)?;
        publish(&staging.0, &destination)?;
        Ok((release, claim))
    })
    .await?;
    Ok(Some(Loaded { directory, release, _claim: claim }))
}

/// Moves the verified `staging` to `destination`, unless a copy is already published there.
fn publish(staging: &Path, destination: &Path) -> io::Result<()> {
    match fs::rename(staging, destination) {
        Err(_) if destination.is_dir() => Ok(()),
        published => published,
    }
}

/// Removes the downloads and unpacks a previous run left unfinished: every entry of `releases` named with a dot.
pub(super) async fn sweep(releases: &Path) -> io::Result<()> {
    let hidden = entries(releases)?.into_iter().filter(|path| hidden(path)).collect();
    remove(hidden).await
}

/// Renames aside the unpacked releases in `releases` whose IDs are neither in `used` nor claimed, returning their new
/// paths.
pub(super) fn set_aside(releases: &Path, claims: &Claims, mut used: BTreeSet<String>) -> Vec<PathBuf> {
    used.extend(claims.used());
    let unused = entries(releases).unwrap_or_default().into_iter().filter(|path| {
        !hidden(path) && path.file_name().and_then(|name| name.to_str()).is_some_and(|name| !used.contains(name))
    });
    unused
        .filter_map(|path| {
            let aside = releases.join(format!(".unused-{}", uuid::Uuid::new_v4()));
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
    blocking(move || {
        for path in paths {
            let removed = if path.is_dir() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) };
            match removed {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        Ok(())
    })
    .await
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

    /// Where control launches the release's JVMs from, with the `java` on the `PATH`.
    pub(super) fn distribution(&self) -> chunk_control::Distribution {
        chunk_control::Distribution { directory: self.directory.clone(), java: "java".into() }
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
            artifact_digest: release.id.clone(),
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

/// An earlier copy of release `id` at `directory` that still verifies. A copy that does not is removed; the caller's
/// claim means no other load uses it.
fn reusable(directory: &Path, id: &str) -> Option<VerifiedRelease> {
    if !directory.exists() {
        return None;
    }
    verify(directory, id)
        .inspect_err(|error| {
            tracing::warn!(%error, release = id, "unpacked release fails verification; downloading it again");
            _ = fs::remove_dir_all(directory);
        })
        .ok()
}

fn verify(directory: &Path, id: &str) -> io::Result<VerifiedRelease> {
    let release = chunk_build::verify_release(directory)
        .map_err(|error| io::Error::other(format!("release {id} fails verification: {error}")))?;
    if release.id != id {
        return Err(io::Error::other(format!("the archive holds release {}, not {id}", release.id)));
    }
    Ok(release)
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
