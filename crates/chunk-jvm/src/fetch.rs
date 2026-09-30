//! Calls to core: what this host runs, that release's archive, and the app's AOT cache.

use crate::{Failure, config::Config};
use chunk_proto::sync::v1::{
    CallRequest, JvmAotUse, JvmAotWrite, JvmArchiveChunk, JvmArchiveRead, JvmBoot, JvmLaunch, call_response::Outcome,
    core_client::CoreClient, error::Code,
};
use prost::Message;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Write},
    path::Path,
    time::Duration,
};
use tokio::time::{Instant, sleep, timeout_at};
use tonic::{metadata::MetadataValue, transport::Channel};

const FIRST_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
/// The most one AOT cache chunk carries, leaving room for its call's framing under core's 1 MiB argument limit.
const UPLOAD_CHUNK: usize = 1024 * 1024 - 1024;

/// Why one attempt failed.
pub(crate) enum Attempt {
    /// Worth retrying after a backoff.
    Transient(String),
    Failed(Failure),
}

impl From<Failure> for Attempt {
    fn from(failure: Failure) -> Self {
        Self::Failed(failure)
    }
}

/// Runs `attempt` until it succeeds or fails for good, backing off after transient failures for up to `budget`. An
/// attempt still unanswered when the budget runs out fails.
pub(crate) async fn retry<T, F: Future<Output = Result<T, Attempt>>>(
    budget: Duration,
    mut attempt: impl FnMut() -> F,
) -> Result<T, Failure> {
    let deadline = Instant::now() + budget;
    let mut backoff = FIRST_BACKOFF;
    loop {
        let Ok(attempted) = timeout_at(deadline, attempt()).await else {
            return Err(Failure::unavailable(format!("core did not answer within {budget:?}")));
        };
        match attempted {
            Ok(value) => return Ok(value),
            Err(Attempt::Failed(failure)) => return Err(failure),
            Err(Attempt::Transient(reason)) if Instant::now() + backoff >= deadline => {
                return Err(Failure::unavailable(format!("core unreachable after {budget:?}: {reason}")));
            }
            Err(Attempt::Transient(reason)) => tracing::debug!(%reason, "core unavailable; retrying"),
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

pub(crate) struct Core {
    client: CoreClient<Channel>,
    bearer: MetadataValue<tonic::metadata::Ascii>,
    retry: Duration,
}

impl Core {
    pub fn new(config: &Config) -> Result<Self, Failure> {
        let channel = Channel::from_shared(config.endpoint.clone()).map_err(Failure::env)?;
        let bearer = format!("Bearer {}", config.credential).parse().map_err(Failure::env)?;
        let client = CoreClient::new(channel.connect_timeout(Duration::from_secs(5)).connect_lazy());
        Ok(Self { client, bearer, retry: config.retry })
    }

    /// Binds this host to `boot`, running Java `runtime`, and returns what it runs.
    pub async fn launch(&self, boot: &str, runtime: &str) -> Result<JvmLaunch, Failure> {
        self.call("chunk:launch", &JvmBoot { boot: boot.into(), runtime: runtime.into() }).await
    }

    /// Downloads the archive `launch` names into `file`, checking its size and SHA-256 as it streams.
    pub async fn download(&self, boot: &str, launch: &JvmLaunch, file: &mut File) -> Result<(), Failure> {
        let (size, sha256) = (launch.archive_size, &launch.archive_sha256);
        self.read_file("chunk:archive", "release archive", boot, (size, sha256), file).await
    }

    /// Downloads the AOT cache `cache` names into `file`, checking its size and SHA-256 as it streams.
    pub async fn download_aot(&self, boot: &str, cache: &JvmAotUse, file: &mut File) -> Result<(), Failure> {
        self.read_file("chunk:aot-read", "AOT cache", boot, (cache.size, &cache.sha256), file).await
    }

    async fn read_file(
        &self,
        method: &str,
        what: &str,
        boot: &str,
        (size, sha256): (u64, &str),
        file: &mut File,
    ) -> Result<(), Failure> {
        let mut digest = Sha256::new();
        let mut offset = 0;
        while offset < size {
            let read = JvmArchiveRead { boot: boot.into(), offset };
            let chunk: JvmArchiveChunk = self.call(method, &read).await?;
            if chunk.data.is_empty() {
                return Err(Failure::verify(format!("core sent no {what} bytes at offset {offset} of {size}")));
            }
            offset += chunk.data.len() as u64;
            if offset > size {
                return Err(Failure::verify(format!("the {what} is larger than its declared {size} bytes")));
            }
            digest.update(&chunk.data);
            file.write_all(&chunk.data).map_err(|error| Failure::io(format!("cannot stage the {what}: {error}")))?;
        }
        let actual = format!("{:x}", digest.finalize());
        if actual != sha256 {
            return Err(Failure::verify(format!("the {what}'s SHA-256 is {actual}, not {sha256}")));
        }
        Ok(())
    }

    /// Uploads the AOT cache at `path`, which this boot recorded, in order.
    pub async fn upload_aot(&self, boot: &str, path: &Path) -> Result<(), Failure> {
        let unreadable = |error: io::Error| Failure::io(format!("cannot read the AOT cache: {error}"));
        let mut file = File::open(path).map_err(unreadable)?;
        let size = file.metadata().map_err(unreadable)?.len();
        let mut digest = Sha256::new();
        io::copy(&mut file, &mut digest).map_err(unreadable)?;
        let sha256 = format!("{:x}", digest.finalize());
        let mut file = File::open(path).map_err(unreadable)?;
        let mut offset = 0;
        while offset < size {
            let mut data = Vec::with_capacity(UPLOAD_CHUNK);
            (&mut file).take(UPLOAD_CHUNK as u64).read_to_end(&mut data).map_err(unreadable)?;
            if data.is_empty() {
                return Err(Failure::io("the AOT cache shrank while it was uploaded"));
            }
            let length = data.len() as u64;
            let write = JvmAotWrite { boot: boot.into(), offset, data, size, sha256: sha256.clone(), abandon: false };
            self.call::<()>("chunk:aot-write", &write).await?;
            offset += length;
        }
        Ok(())
    }

    /// Tells core this boot made no AOT cache, so another host may.
    pub async fn abandon_aot(&self, boot: &str) -> Result<(), Failure> {
        self.call("chunk:aot-write", &JvmAotWrite { boot: boot.into(), abandon: true, ..JvmAotWrite::default() }).await
    }

    async fn call<T: Message + Default>(&self, method: &str, arguments: &impl Message) -> Result<T, Failure> {
        let arguments = arguments.encode_to_vec();
        retry(self.retry, || async {
            let mut request = tonic::Request::new(CallRequest {
                method: method.into(),
                arguments: arguments.clone(),
                ..CallRequest::default()
            });
            request.metadata_mut().insert("authorization", self.bearer.clone());
            let response = match self.client.clone().call(request).await {
                Ok(response) => response.into_inner(),
                Err(status) if status.code() == tonic::Code::Unauthenticated => {
                    return Err(Failure::refused("core rejected the JVM credential").into());
                }
                Err(status) => return Err(Attempt::Transient(status.to_string())),
            };
            match response.outcome {
                Some(Outcome::Result(result)) => {
                    T::decode(result.as_slice()).map_err(|error| Failure::unavailable(error).into())
                }
                Some(Outcome::Error(error)) => {
                    let message = format!("{method} failed with {:?}: {}", error.code(), error.message);
                    Err(match error.code() {
                        Code::Unavailable | Code::Overloaded => Attempt::Transient(message),
                        Code::Denied | Code::Stopped => Failure::refused(message).into(),
                        _ => Failure::unavailable(message).into(),
                    })
                }
                None => Err(Failure::unavailable(format!("{method} returned no outcome")).into()),
            }
        })
        .await
    }
}
