//! The Java command that runs a verified release's app.

use crate::{Failure, config::Config};
use chunk_build::VerifiedRelease;
use chunk_proto::sync::v1::JvmLaunch;
use sha2::{Digest, Sha256};
use std::{
    fs,
    net::IpAddr,
    path::Path,
    process::{Command, Stdio},
};
use tempfile::TempDir;

/// Memory the JVM needs outside its heap: this much, plus a tenth of the machine's memory.
const RESERVE_MIB: u64 = 200;
/// The smallest heap the runner starts a JVM with.
const MIN_HEAP_MIB: u64 = 64;

pub(crate) struct Jvm {
    pub command: Command,
    /// The JVM's working directory, removed once dropped.
    _directory: TempDir,
}

/// Checks that the image can run `launch`'s app from `release`, installed at `directory`, and builds its command.
pub(crate) fn prepare(
    config: &Config,
    launch: &JvmLaunch,
    release: &VerifiedRelease,
    directory: &Path,
    player_address: IpAddr,
) -> Result<Jvm, Failure> {
    let app = release
        .apps
        .iter()
        .find(|app| app.id == launch.app)
        .ok_or_else(|| Failure::verify(format!("release {} has no app {:?}", release.id, launch.app)))?;
    let image = java_major(&config.java_home)?;
    if release.java_version > image {
        return Err(Failure::java(format!(
            "release {} needs Java {}, but this image has Java {image}",
            release.id, release.java_version
        )));
    }
    let profile = release.profiles.get(&launch.profile).map(|profile| u64::from(profile.memory_mib));
    let heap = heap_mib(visible_mib(&config.memory_max, &config.meminfo), profile)?;
    let jar = checked_jar(directory, &app.jar, &app.sha256)?;
    let working = tempfile::Builder::new()
        .prefix("chunk-jvm-")
        .tempdir_in(&config.work_root)
        .map_err(|error| Failure::io(format!("cannot create the JVM's working directory: {error}")))?;
    let mut command = Command::new(config.java_home.join("bin/java"));
    command
        .arg(format!("-Xmx{heap}m"))
        .args(["-XX:+UseG1GC", "-XX:+ExitOnOutOfMemoryError", "-jar"])
        .arg(jar)
        .current_dir(working.path())
        .env("CHUNK_PROCESS_TOKEN", &config.credential)
        .env("CHUNK_ENVIRONMENT", &config.environment)
        .env("CHUNK_DEPLOYMENT", &launch.deployment)
        .env("CHUNK_CORE_ENDPOINT", &config.endpoint)
        // The endpoint's older name, for runtime JARs that predate `CHUNK_CORE_ENDPOINT`.
        .env("CHUNK_CONTROL_ENDPOINT", &config.endpoint)
        .env("CHUNK_INSTANCE_ID", &config.host)
        .env("CHUNK_PROCESS_ID", &launch.process_id)
        .env("CHUNK_PROCESS_GENERATION", launch.generation.to_string())
        .env("CHUNK_MACHINE_PROFILE", &launch.profile)
        .env("CHUNK_APP_ID", &launch.app)
        .env("CHUNK_ARTIFACT_DIGEST", &app.sha256)
        .env("CHUNK_PLAYER_ADDRESS", player_address.to_string())
        .stdin(Stdio::null());
    tracing::info!(heap_mib = heap, java = image, "starting the JVM");
    Ok(Jvm { command, _directory: working })
}

/// The app JAR at `jar` within `directory`, once its contents hash to `sha256`.
fn checked_jar(directory: &Path, jar: &str, sha256: &str) -> Result<std::path::PathBuf, Failure> {
    let unreadable = |error: std::io::Error| Failure::verify(format!("cannot read the app JAR {jar}: {error}"));
    let root = directory.canonicalize().map_err(unreadable)?;
    let path = directory.join(jar).canonicalize().map_err(unreadable)?;
    let bytes = fs::read(&path).map_err(unreadable)?;
    if !path.starts_with(&root) || format!("{:x}", Sha256::digest(&bytes)) != sha256 {
        return Err(Failure::verify(format!("the app JAR {jar} differs from its digest")));
    }
    Ok(path)
}

/// The Java major version of the image, from `JAVA_VERSION` in `$JAVA_HOME/release`.
fn java_major(java_home: &Path) -> Result<u32, Failure> {
    let path = java_home.join("release");
    let unknown = |reason: String| Failure::java(format!("cannot tell this image's Java version: {reason}"));
    let release = fs::read_to_string(&path).map_err(|error| unknown(format!("{}: {error}", path.display())))?;
    release
        .lines()
        .find_map(|line| line.strip_prefix("JAVA_VERSION="))
        .and_then(parse_major)
        .ok_or_else(|| unknown(format!("{} names no JAVA_VERSION", path.display())))
}

/// The major version in a `JAVA_VERSION` value such as `"25"`, `"21.0.4"` or `"1.8.0_392"`.
fn parse_major(value: &str) -> Option<u32> {
    let value = value.trim().trim_matches('"');
    let value = value.strip_prefix("1.").unwrap_or(value);
    let digits = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    value[..digits].parse().ok()
}

/// The memory this machine lets the runner use, in MiB: the lower of the cgroup v2 limit at `memory_max` and
/// `MemTotal` in `meminfo`, whichever are known.
fn visible_mib(memory_max: &Path, meminfo: &Path) -> Option<u64> {
    let limit = fs::read_to_string(memory_max).ok().and_then(|limit| limit.trim().parse::<u64>().ok());
    let total = fs::read_to_string(meminfo).ok().and_then(|meminfo| {
        let line = meminfo.lines().find_map(|line| line.strip_prefix("MemTotal:"))?;
        line.trim().strip_suffix("kB")?.trim().parse::<u64>().ok().map(|kib| kib * 1024)
    });
    [limit, total].into_iter().flatten().min().map(|bytes| bytes / (1024 * 1024))
}

/// The heap for a machine with `visible` MiB, capped by its profile's `profile` MiB: that memory less the non-heap
/// reserve.
fn heap_mib(visible: Option<u64>, profile: Option<u64>) -> Result<u64, Failure> {
    let memory = [visible, profile]
        .into_iter()
        .flatten()
        .min()
        .ok_or_else(|| Failure::env("no memory limit is visible and the launch's profile names no memory"))?;
    let heap = memory.saturating_sub(RESERVE_MIB + memory / 10);
    if heap < MIN_HEAP_MIB {
        return Err(Failure::env(format!("{memory} MiB of memory leaves less than {MIN_HEAP_MIB} MiB of heap")));
    }
    Ok(heap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_versions_parse_to_their_major() {
        for (value, major) in [("\"25\"", Some(25)), ("\"21.0.4\"", Some(21)), ("\"1.8.0_392\"", Some(8))] {
            assert_eq!(parse_major(value), major, "{value}");
        }
        assert_eq!(parse_major("\"26-ea\""), Some(26));
        assert_eq!(parse_major("\"\""), None);
    }

    #[test]
    fn the_heap_is_the_visible_or_profile_memory_less_the_reserve() {
        let directory = tempfile::tempdir().unwrap();
        let (memory_max, meminfo) = (directory.path().join("memory.max"), directory.path().join("meminfo"));
        assert_eq!(visible_mib(&memory_max, &meminfo), None);
        fs::write(&meminfo, "MemFree:  100 kB\nMemTotal:       2097152 kB\n").unwrap();
        assert_eq!(visible_mib(&memory_max, &meminfo), Some(2048));
        fs::write(&memory_max, "max\n").unwrap();
        assert_eq!(visible_mib(&memory_max, &meminfo), Some(2048));
        fs::write(&memory_max, "1073741824\n").unwrap();
        assert_eq!(visible_mib(&memory_max, &meminfo), Some(1024));

        assert_eq!(heap_mib(Some(1024), None).unwrap(), 1024 - 200 - 102);
        assert_eq!(heap_mib(Some(1024), Some(512)).unwrap(), 512 - 200 - 51);
        assert_eq!(heap_mib(None, Some(4096)).unwrap(), 4096 - 200 - 409);
        assert_eq!(heap_mib(None, None).unwrap_err().code, 64);
        assert_eq!(heap_mib(Some(256), None).unwrap_err().code, 64);
    }
}
