//! What a remote runner starts on its host, `chunk:launch`, that host's release archive, `chunk:archive`, its app's
//! asset blobs, `chunk:asset-read`, and its AOT cache, `chunk:aot-read` and `chunk:aot-write`. Only the host's JVM
//! machine credential calls them.

use super::{
    super::{
        SyncService,
        auth::{Class, Principal},
        errors,
    },
    decode,
};
use crate::core::{Archives, ReleaseArchive};
use chunk_build::assets::Store;
use chunk_contract::AssetRevision;
use chunk_proto::sync::v1::{
    CallRequest, Error, JvmAotWrite, JvmArchiveChunk, JvmArchiveRead, JvmAssetRead, JvmAssets, JvmBoot, JvmLaunch,
    Position, error::Code,
};
use prost::Message;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

/// The most one archive chunk carries, leaving room for the response's framing under tonic's default 4 MiB limit.
const CHUNK_BYTES: usize = 4 * 1024 * 1024 - 1024;
const BOOT_BYTES: usize = 128;
/// The most asset revisions core keeps decoded at once.
const REVISIONS: usize = 16;

#[derive(Clone, Copy)]
pub(super) enum Method {
    Launch,
    Archive,
    AssetRead,
    AotRead,
    AotWrite,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "launch" => Self::Launch,
            "archive" => Self::Archive,
            "asset-read" => Self::AssetRead,
            "aot-read" => Self::AotRead,
            "aot-write" => Self::AotWrite,
            _ => return None,
        })
    }
}

/// Runs `method` for the host whose machine credential `principal` presents.
pub(super) async fn call(
    service: &SyncService,
    principal: &Principal,
    method: Method,
    request: &CallRequest,
) -> Result<(Option<Position>, Vec<u8>), Error> {
    let denied = || errors::denied("only the host's JVM machine credential launches it");
    let Class::Jvm { host } = &principal.class else { return Err(denied()) };
    if !service.credentials.jvm_machine(principal) {
        return Err(denied());
    }
    if !request.deployment.is_empty() || request.caller.is_some() || !request.stream.is_empty() {
        return Err(errors::invalid("a runner method takes no deployment, caller or stream"));
    }
    let bound = |boot: &str| {
        let launch = service.control.launch(host).filter(|launch| launch.boot.as_deref() == Some(boot));
        launch.ok_or_else(|| errors::denied("only the boot bound to the host reads or writes its files"))
    };
    let result = match method {
        Method::Launch => {
            let JvmBoot { boot, runtime } = decode(&request.arguments)?;
            if boot.is_empty() || boot.len() > BOOT_BYTES {
                return Err(errors::invalid("a boot ID is 1 to 128 bytes"));
            }
            let launch = service.control.boot_launch(host, &boot).map_err(|failure| errors::control(&failure))?;
            let archive = service.archives.archive(&launch.release)?;
            let assets = &release(service, &launch.deployment)?.assets;
            let manifest = service.archives.revision(assets)?.encode();
            let assets = JvmAssets { revision_id: assets.revision_id.clone(), manifest };
            let aot = service.aot.plan(host, &boot, &launch.release, &launch.app, &runtime).await;
            JvmLaunch {
                deployment: launch.deployment,
                release_id: launch.release,
                archive_size: archive.size,
                archive_sha256: archive.sha256,
                app: launch.app,
                profile: launch.profile,
                process_id: launch.process_id,
                generation: launch.generation,
                aot,
                environment_name: service.environment_name.clone(),
                assets: Some(assets),
            }
            .encode_to_vec()
        }
        Method::Archive => {
            let read: JvmArchiveRead = decode(&request.arguments)?;
            let launch = bound(&read.boot)?;
            let archive = service.archives.archive(&launch.release)?;
            JvmArchiveChunk { data: service.archives.read(host, archive, read.offset).await? }.encode_to_vec()
        }
        Method::AssetRead => {
            let read: JvmAssetRead = decode(&request.arguments)?;
            let launch = bound(&read.boot)?;
            let revision = service.archives.revision(&release(service, &launch.deployment)?.assets)?;
            let size = revision.app_blobs(&launch.app).get(read.sha256.as_str()).copied();
            let size = size.ok_or_else(|| errors::denied("the blob is not one the host's app reads"))?;
            let blob = service.archives.blob(read.sha256, size)?;
            JvmArchiveChunk { data: service.archives.read(host, blob, read.offset).await? }.encode_to_vec()
        }
        Method::AotRead => {
            let read: JvmArchiveRead = decode(&request.arguments)?;
            bound(&read.boot)?;
            let cache = service.aot.used(host, &read.boot);
            let cache = cache.ok_or_else(|| errors::error(Code::Contract, "core offered the host no AOT cache"))?;
            let file = ReleaseArchive { path: cache.path, sha256: cache.sha256, size: cache.size };
            JvmArchiveChunk { data: service.archives.read(host, file, read.offset).await? }.encode_to_vec()
        }
        Method::AotWrite => {
            let write: JvmAotWrite = decode(&request.arguments)?;
            bound(&write.boot)?;
            service.aot.write(host, &write.boot.clone(), write).await?;
            Vec::new()
        }
    };
    Ok((None, result))
}

/// The release of `deployment`, which a host's launch names.
fn release(service: &SyncService, deployment: &str) -> Result<Arc<chunk_control::Release>, Error> {
    let release = service.control.release(deployment).map_err(|failure| errors::control(&failure))?;
    release.ok_or_else(|| errors::error(Code::Contract, "core knows no release of the host's deployment"))
}

/// Core's kept release archives and asset store, the asset revisions it read last, and the hosts reading a chunk of
/// one of their files now.
pub(in super::super) struct ArchiveReads {
    archives: Arc<Archives>,
    assets: Store,
    revisions: Mutex<BTreeMap<String, Arc<AssetRevision>>>,
    reading: Arc<Mutex<BTreeSet<String>>>,
}

impl ArchiveReads {
    pub fn new(archives: Arc<Archives>, assets: Store) -> Self {
        Self { archives, assets, revisions: Mutex::default(), reading: Arc::default() }
    }

    /// The asset revision a deployment pins, which core wrote to its store before activating it.
    fn revision(&self, assets: &chunk_control::DeploymentAssets) -> Result<Arc<AssetRevision>, Error> {
        if let Some(revision) = lock(&self.revisions).get(&assets.revision_id) {
            return Ok(revision.clone());
        }
        let revision = assets.revision(&self.assets).map_err(|error| {
            errors::error(Code::Contract, format!("core cannot read the host's asset revision: {error}"))
        })?;
        let revision = Arc::new(revision);
        let mut revisions = lock(&self.revisions);
        if revisions.len() >= REVISIONS {
            revisions.clear();
        }
        revisions.insert(assets.revision_id.clone(), revision.clone());
        Ok(revision)
    }

    fn archive(&self, release: &str) -> Result<ReleaseArchive, Error> {
        let archive = self.archives.get(release);
        archive.ok_or_else(|| errors::error(Code::Contract, "core keeps no archive of the host's release"))
    }

    /// The asset blob with this SHA-256 and size, which the store checked as it stored it.
    fn blob(&self, sha256: String, size: u64) -> Result<ReleaseArchive, Error> {
        if !self.assets.contains(&sha256).unwrap_or(false) {
            return Err(errors::error(Code::Contract, "core holds no such asset blob"));
        }
        let path = self.assets.root().join("blobs").join(&sha256);
        Ok(ReleaseArchive { path, sha256, size })
    }

    /// The chunk of `archive`, or of another file core checked the same way, at `offset`, read for `host` unless
    /// another of its reads is running.
    async fn read(&self, host: &str, archive: ReleaseArchive, offset: u64) -> Result<Vec<u8>, Error> {
        if offset >= archive.size {
            return Err(errors::invalid("the offset is at or past the archive's end"));
        }
        if !lock(&self.reading).insert(host.to_owned()) {
            return Err(errors::error(Code::Overloaded, "another archive read of this host is running"));
        }
        let reading = Reading { hosts: self.reading.clone(), host: host.to_owned() };
        #[cfg(test)]
        let stall = self.archives.stall.clone();
        let read = tokio::task::spawn_blocking(move || {
            // The host's read ends only once the file is no longer being read, even if its call was dropped.
            let _reading = reading;
            #[cfg(test)]
            {
                stall.entered.notify_one();
                drop(stall.held.blocking_lock());
            }
            let length = usize::try_from(archive.size - offset).unwrap_or(usize::MAX).min(CHUNK_BYTES);
            let mut file = File::open(&archive.path)?;
            file.seek(SeekFrom::Start(offset))?;
            let mut data = vec![0; length];
            file.read_exact(&mut data)?;
            Ok::<_, io::Error>(data)
        });
        match read.await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(error)) => Err(errors::error(Code::Unavailable, format!("reading the archive failed: {error}"))),
            Err(_) => Err(errors::error(Code::Unavailable, "reading the archive failed")),
        }
    }
}

/// A host's running archive read, which ends when this drops.
struct Reading {
    hosts: Arc<Mutex<BTreeSet<String>>>,
    host: String,
}

impl Drop for Reading {
    fn drop(&mut self) {
        lock(&self.hosts).remove(&self.host);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
