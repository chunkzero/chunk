//! What a remote runner starts on its host, `chunk:launch`, and that host's release archive, `chunk:archive`. Only the
//! host's JVM machine credential calls them.

use super::{
    super::{
        SyncService,
        auth::{Class, Principal},
        errors,
    },
    decode,
};
use crate::core::{Archives, ReleaseArchive};
use chunk_proto::sync::v1::{
    CallRequest, Error, JvmArchiveChunk, JvmArchiveRead, JvmBoot, JvmLaunch, Position, error::Code,
};
use prost::Message;
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};

/// The most one archive chunk carries.
const CHUNK_BYTES: usize = 4 * 1024 * 1024;
const BOOT_BYTES: usize = 128;

#[derive(Clone, Copy)]
pub(super) enum Method {
    Launch,
    Archive,
}

impl Method {
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "launch" => Self::Launch,
            "archive" => Self::Archive,
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
    let result = match method {
        Method::Launch => {
            let JvmBoot { boot } = decode(&request.arguments)?;
            if boot.is_empty() || boot.len() > BOOT_BYTES {
                return Err(errors::invalid("a boot ID is 1 to 128 bytes"));
            }
            let launch = service.control.boot_launch(host, &boot).map_err(|failure| errors::control(&failure))?;
            let archive = service.archives.archive(&launch.release)?;
            JvmLaunch {
                deployment: launch.deployment,
                release_id: launch.release,
                archive_size: archive.size,
                archive_sha256: archive.sha256,
                app: launch.app,
                profile: launch.profile,
                process_id: launch.process_id,
                generation: launch.generation,
            }
            .encode_to_vec()
        }
        Method::Archive => {
            let read: JvmArchiveRead = decode(&request.arguments)?;
            let launch = service.control.launch(host).filter(|launch| launch.boot.as_ref() == Some(&read.boot));
            let launch = launch.ok_or_else(|| errors::denied("only the boot bound to the host reads its archive"))?;
            let archive = service.archives.archive(&launch.release)?;
            JvmArchiveChunk { data: service.archives.read(host, archive, read.offset).await? }.encode_to_vec()
        }
    };
    Ok((None, result))
}

/// Core's kept release archives, and the hosts reading a chunk of one now.
pub(in super::super) struct ArchiveReads {
    archives: Arc<Archives>,
    reading: Arc<Mutex<BTreeSet<String>>>,
}

impl ArchiveReads {
    pub fn new(archives: Arc<Archives>) -> Self {
        Self { archives, reading: Arc::default() }
    }

    fn archive(&self, release: &str) -> Result<ReleaseArchive, Error> {
        let archive = self.archives.get(release);
        archive.ok_or_else(|| errors::error(Code::Contract, "core keeps no archive of the host's release"))
    }

    /// The chunk of `archive` at `offset`, read for `host` unless another of its reads is running.
    async fn read(&self, host: &str, archive: ReleaseArchive, offset: u64) -> Result<Vec<u8>, Error> {
        if offset >= archive.size {
            return Err(errors::invalid("the offset is at or past the archive's end"));
        }
        if !lock(&self.reading).insert(host.to_owned()) {
            return Err(errors::error(Code::Overloaded, "another archive read of this host is running"));
        }
        let reading = Reading { hosts: self.reading.clone(), host: host.to_owned() };
        let read = tokio::task::spawn_blocking(move || {
            // The host's read ends only once the file is no longer being read, even if its call was dropped.
            let _reading = reading;
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

fn lock(hosts: &Mutex<BTreeSet<String>>) -> MutexGuard<'_, BTreeSet<String>> {
    hosts.lock().unwrap_or_else(PoisonError::into_inner)
}
