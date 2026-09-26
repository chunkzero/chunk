//! Loading the releases the management service deploys, checked with `chunk build`'s own rules.

use chunk_build::{ArchiveDigest, UnpackLimits, VerifiedRelease};
use chunk_management::{Client, v1};
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::io::AsyncWriteExt;

const DOWNLOAD_ATTEMPTS: u32 = 3;
/// JVMs one release may run at once: control's limit, since capacity bounds them.
const MAX_PROCESSES: u16 = 32;

/// A verified release, unpacked under the state directory.
pub(super) struct Loaded {
    directory: PathBuf,
    release: VerifiedRelease,
}

/// Unpacks and verifies the release `artifact` names into `releases/<release_id>`, reusing an earlier copy there that
/// still verifies.
pub(super) async fn load(client: &Client, releases: &Path, artifact: &v1::ReleaseArtifact) -> io::Result<Loaded> {
    let id = artifact.release_id.clone();
    let plain = !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if !plain {
        return Err(io::Error::other("the release ID is not a plain name"));
    }
    fs::create_dir_all(releases)?;
    let directory = releases.join(&id);
    let (existing, expected) = (directory.clone(), id.clone());
    if let Some(release) = blocking(move || Ok(reusable(&existing, &expected))).await? {
        return Ok(Loaded { directory, release });
    }
    let archive = releases.join(format!(".{id}.tar.gz"));
    if let Err(error) = download(client, artifact, &archive).await {
        _ = fs::remove_file(&archive);
        return Err(error);
    }
    let digest = ArchiveDigest { sha256: artifact.sha256.clone(), size: artifact.size_bytes };
    let destination = directory.clone();
    let release = blocking(move || {
        let unpacked = chunk_build::unpack_release(&archive, &digest, &destination, &UnpackLimits::default());
        _ = fs::remove_file(&archive);
        unpacked?;
        verify(&destination, &id).inspect_err(|_| _ = fs::remove_dir_all(&destination))
    })
    .await?;
    Ok(Loaded { directory, release })
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
            deployment: chunk_proto::v1::DeploymentRef {
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

/// An earlier copy of release `id` at `directory` that still verifies. A copy that does not is removed.
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

/// Downloads the archive to `path`, retrying failed transfers, and stops early once it outgrows its declared size.
async fn download(client: &Client, artifact: &v1::ReleaseArtifact, path: &Path) -> io::Result<()> {
    let mut attempt = 1;
    loop {
        match download_once(client, artifact, path).await {
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
    Transfer(chunk_management::Error),
    Local(io::Error),
}

async fn download_once(client: &Client, artifact: &v1::ReleaseArtifact, path: &Path) -> Result<(), Failure> {
    let mut download = client.download_archive(&artifact.url).await.map_err(Failure::Transfer)?;
    let mut file = tokio::fs::File::create(path).await.map_err(Failure::Local)?;
    let mut size = 0;
    while let Some(chunk) = download.chunk().await.map_err(Failure::Transfer)? {
        size += chunk.len() as u64;
        if size > artifact.size_bytes {
            return Err(Failure::Local(io::Error::other("the release archive is larger than its declared size")));
        }
        file.write_all(&chunk).await.map_err(Failure::Local)?;
    }
    file.flush().await.map_err(Failure::Local)
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> io::Result<T> + Send + 'static) -> io::Result<T> {
    tokio::task::spawn_blocking(work).await.map_err(io::Error::other)?
}
