//! The JVM's Leyden AOT cache. Core names a cache for the JVM to start with, or asks this runner to record the JVM's run,
//! create the cache once it exits cleanly, and upload it. Any failure leaves the JVM to run, or the runner to exit, as it
//! would without a cache.

use crate::{Failure, cache::Cache, fetch, launch::Jvm};
use chunk_proto::sync::v1::{JvmAotUse, JvmLaunch, jvm_launch::Aot};
use rustix::process::Signal;
use sha2::{Digest, Sha256};
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tempfile::TempDir;
use tokio::sync::mpsc;

// Together these stay under the 90 seconds core holds a recording host's release for.
/// How long Java may take to create the cache.
const CREATE_TIMEOUT: Duration = Duration::from_secs(45);
/// How long the upload may take, and telling core that no cache came.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30);
const ABANDON_TIMEOUT: Duration = Duration::from_secs(10);
/// How long fetching a cache to use may take in all, well inside the host's readiness deadline.
const FETCH_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) enum Plan {
    None,
    /// Start with the cache at this path.
    Use(PathBuf),
    /// Record the run into a configuration file in this directory, and create the cache there.
    Record(TempDir),
}

impl Plan {
    /// Makes `launch`'s plan ready: fetches the cache to use into `cache`, or makes the directory a recording writes to
    /// in `work_root`. A cache that can't be fetched is logged and left out.
    pub async fn prepare(
        core: &fetch::Core,
        boot: &str,
        launch: &JvmLaunch,
        cache: &Cache,
        work_root: &Path,
    ) -> Result<Self, Failure> {
        match &launch.aot {
            None => Ok(Self::None),
            Some(Aot::Use(wanted)) => {
                match tokio::time::timeout(FETCH_TIMEOUT, fetch(core, boot, launch, cache, wanted)).await {
                    Ok(Ok(path)) => Ok(Self::Use(path)),
                    Ok(Err(failure)) => {
                        tracing::warn!(message = failure.message, "cannot fetch the AOT cache; running without it");
                        Ok(Self::None)
                    }
                    Err(_) => {
                        tracing::warn!("fetching the AOT cache took longer than {FETCH_TIMEOUT:?}; running without it");
                        Ok(Self::None)
                    }
                }
            }
            Some(Aot::Record(_)) => {
                let directory = tempfile::Builder::new().prefix("chunk-aot-").tempdir_in(work_root);
                let directory =
                    directory.map_err(|error| Failure::io(format!("cannot make the AOT directory: {error}")))?;
                Ok(Self::Record(directory))
            }
        }
    }

    /// The flags that start the JVM this way.
    pub fn flags(&self) -> Vec<OsString> {
        match self {
            Self::None => Vec::new(),
            Self::Use(cache) => vec![flag("-XX:AOTCache=", cache)],
            Self::Record(directory) => {
                vec!["-XX:AOTMode=record".into(), flag("-XX:AOTConfiguration=", &configuration(directory.path()))]
            }
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Use(_) => "use",
            Self::Record(_) => "record",
        }
    }
}

/// `name` followed by `path`, as one argument.
pub(crate) fn flag(name: &str, path: &Path) -> OsString {
    let mut flag = OsString::from(name);
    flag.push(path);
    flag
}

fn configuration(directory: &Path) -> PathBuf {
    directory.join("app.aotconf")
}

/// The cache `wanted` names, kept for `launch`'s release and app in `cache`: the one there once it matches, or else
/// downloaded again.
async fn fetch(
    core: &fetch::Core,
    boot: &str,
    launch: &JvmLaunch,
    cache: &Cache,
    wanted: &JvmAotUse,
) -> Result<PathBuf, Failure> {
    let path = cache.aot(launch)?;
    if matches(&path, wanted).unwrap_or(false) {
        tracing::info!("reusing the cached AOT cache");
        return Ok(path);
    }
    tracing::info!(size = wanted.size, "downloading the AOT cache");
    let mut staging = cache.aot_staging()?;
    core.download_aot(boot, wanted, staging.as_file_mut()).await?;
    let io = |error: io::Error| Failure::io(format!("cannot keep the AOT cache: {error}"));
    fs::create_dir_all(path.parent().unwrap_or(&path)).map_err(io)?;
    staging.persist(&path).map_err(|error| io(error.error))?;
    Ok(path)
}

fn matches(path: &Path, wanted: &JvmAotUse) -> io::Result<bool> {
    let mut file = File::open(path)?;
    if file.metadata()?.len() != wanted.size {
        return Ok(false);
    }
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()) == wanted.sha256)
}

/// Creates the cache from the recording in `directory` of `jvm`'s run once it exited `cleanly`, and uploads it. Any
/// failure, or a SIGTERM or SIGINT meanwhile, is logged, and core told that no cache came.
pub(crate) async fn finish(
    core: &fetch::Core,
    boot: &str,
    jvm: &Jvm,
    directory: &Path,
    cleanly: bool,
    signals: &mut mpsc::UnboundedReceiver<Signal>,
) {
    let made = async {
        if !cleanly {
            return Err("the JVM did not exit cleanly".to_owned());
        }
        let cache = directory.join("app.aot");
        create(jvm, directory, &cache).await?;
        let uploading = Instant::now();
        let uploaded = tokio::time::timeout(UPLOAD_TIMEOUT, core.upload_aot(boot, &cache)).await;
        match uploaded {
            Ok(Ok(())) => {
                tracing::info!(elapsed = ?uploading.elapsed(), "uploaded the AOT cache");
                Ok(())
            }
            Ok(Err(failure)) => Err(format!("cannot upload the AOT cache: {}", failure.message)),
            Err(_) => Err(format!("uploading the AOT cache took longer than {UPLOAD_TIMEOUT:?}")),
        }
    };
    let made = tokio::select! {
        made = made => made,
        () = crate::stopped(signals) => Err("the runner was stopped".to_owned()),
    };
    if let Err(reason) = made {
        tracing::warn!(reason, "made no AOT cache");
        abandon(core, boot).await;
    }
}

/// Tells core, if it answers soon, that this boot makes no AOT cache, so its release needn't wait and another host may
/// record it.
pub(crate) async fn abandon(core: &fetch::Core, boot: &str) {
    if let Ok(Err(failure)) = tokio::time::timeout(ABANDON_TIMEOUT, core.abandon_aot(boot)).await {
        tracing::debug!(message = failure.message, "core did not take the abandoned AOT recording");
    }
}

/// Runs Java's create step for `jvm` from the configuration recorded in `directory` into `cache`.
async fn create(jvm: &Jvm, directory: &Path, cache: &Path) -> Result<(), String> {
    let creating = Instant::now();
    let mut command = tokio::process::Command::from(jvm.create(&configuration(directory), cache));
    command.kill_on_drop(true);
    let status = match tokio::time::timeout(CREATE_TIMEOUT, command.status()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => return Err(format!("cannot run Java to create the AOT cache: {error}")),
        Err(_) => return Err(format!("creating the AOT cache took longer than {CREATE_TIMEOUT:?}")),
    };
    if !status.success() {
        return Err(format!("creating the AOT cache failed: Java {status}"));
    }
    let size = fs::metadata(cache).map_err(|error| format!("Java created no AOT cache: {error}"))?.len();
    tracing::info!(size, elapsed = ?creating.elapsed(), "created the AOT cache");
    Ok(())
}
