//! The Java command that runs a verified release's app.

use crate::{Failure, aot, config::Config, memory};
use chunk_build::VerifiedRelease;
use chunk_proto::sync::v1::JvmLaunch;
use std::{
    fs,
    net::IpAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
use tempfile::TempDir;

/// Memory the JVM needs outside its heap: this much, plus a tenth of the machine's memory.
const RESERVE_MIB: u64 = 200;
/// The smallest heap the runner starts a JVM with.
const MIN_HEAP_MIB: u64 = 64;

pub(crate) struct Jvm {
    pub command: Command,
    /// What the JVM does about its AOT cache.
    pub aot: aot::Plan,
    /// Java, and the flags and JAR it runs the app with, which creating an AOT cache repeats.
    java: PathBuf,
    flags: Vec<String>,
    jar: PathBuf,
    /// The JVM's working directory, removed once dropped.
    directory: TempDir,
}

impl Jvm {
    /// The Java command that creates the AOT cache `cache` from the configuration a recording run wrote to
    /// `configuration`.
    pub fn create(&self, configuration: &Path, cache: &Path) -> Command {
        let mut command = Command::new(&self.java);
        command
            .args(&self.flags)
            .args([
                "-XX:AOTMode=create".into(),
                aot::flag("-XX:AOTConfiguration=", configuration),
                aot::flag("-XX:AOTCache=", cache),
            ])
            .arg("-jar")
            .arg(&self.jar)
            .current_dir(self.directory.path())
            .stdin(Stdio::null());
        command
    }
}

/// Checks that the image can run `launch`'s app from `release`, installed at `directory`, and builds its command,
/// which follows `aot`.
pub(crate) fn prepare(
    config: &Config,
    launch: &JvmLaunch,
    release: &VerifiedRelease,
    directory: &Path,
    player_address: IpAddr,
    aot: aot::Plan,
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
    let heap = heap_mib(memory::visible_mib(&config.proc), profile)?;
    let jar = app_jar(directory, &app.jar)?;
    let working = tempfile::Builder::new()
        .prefix("chunk-jvm-")
        .tempdir_in(&config.work_root)
        .map_err(|error| Failure::io(format!("cannot create the JVM's working directory: {error}")))?;
    let java = config.java_home.join("bin/java");
    let flags = flags(heap, config.cpus, matches!(aot, aot::Plan::Record(_)));
    let mut command = Command::new(&java);
    command
        .args(&flags)
        .args(aot.flags())
        .arg("-jar")
        .arg(&jar)
        .current_dir(working.path())
        .env("CHUNK_PROCESS_TOKEN", &config.credential)
        .env("CHUNK_DEPLOYMENT", &launch.deployment)
        .env("CHUNK_CORE_ENDPOINT", &config.endpoint)
        .env("CHUNK_PROCESS_ID", &launch.process_id)
        .env("CHUNK_PROCESS_GENERATION", launch.generation.to_string())
        .env("CHUNK_MACHINE_PROFILE", &launch.profile)
        .env("CHUNK_APP_ID", &launch.app)
        .env("CHUNK_ARTIFACT_DIGEST", &app.sha256)
        .env("CHUNK_PLAYER_ADDRESS", player_address.to_string())
        .stdin(Stdio::null());
    tracing::info!(heap_mib = heap, cpus = config.cpus, java = image, aot = aot.name(), "starting the JVM");
    Ok(Jvm { command, aot, java, flags, jar, directory: working })
}

/// The JVM's heap and collector flags for `heap` MiB on `cpus` CPUs. The heap starts at its full size, which spares a
/// starting JVM the collections that grow it, unless the JVM is `recording` its AOT cache: finishing a recording needs
/// the memory a heap that grows only as needed leaves. One CPU runs the serial collector, whose work doesn't compete
/// with the app's from other threads.
fn flags(heap: u64, cpus: usize, recording: bool) -> Vec<String> {
    let collector = if cpus == 1 { "-XX:+UseSerialGC" } else { "-XX:+UseG1GC" };
    let initial = (!recording).then(|| format!("-Xms{heap}m"));
    let rest = [format!("-Xmx{heap}m"), collector.into(), "-XX:+ExitOnOutOfMemoryError".into()];
    initial.into_iter().chain(rest).collect()
}

/// The app JAR at `jar` within `directory`. Its contents are the release's: they came in the archive core named, and
/// a cached install verifies before it is reused.
fn app_jar(directory: &Path, jar: &str) -> Result<PathBuf, Failure> {
    let unreadable = |error: std::io::Error| Failure::verify(format!("cannot read the app JAR {jar}: {error}"));
    let root = directory.canonicalize().map_err(unreadable)?;
    let path = directory.join(jar).canonicalize().map_err(unreadable)?;
    if !path.starts_with(&root) {
        return Err(Failure::verify(format!("the app JAR {jar} is outside the release")));
    }
    Ok(path)
}

/// The Java major version of the image, from `JAVA_VERSION` in `$JAVA_HOME/release`.
fn java_major(java_home: &Path) -> Result<u32, Failure> {
    let path = java_home.join("release");
    let unknown = |reason: String| Failure::java(format!("cannot tell this image's Java version: {reason}"));
    let release = fs::read_to_string(&path).map_err(|error| unknown(format!("{}: {error}", path.display())))?;
    value(&release, "JAVA_VERSION")
        .and_then(parse_major)
        .ok_or_else(|| unknown(format!("{} names no JAVA_VERSION", path.display())))
}

/// The image's Java runtime as `$JAVA_HOME/release` names it, which an AOT cache must match: its `IMPLEMENTOR`,
/// `JAVA_RUNTIME_VERSION` and `OS_ARCH`. Empty when it names no runtime version.
pub(crate) fn runtime(java_home: &Path) -> String {
    let release = fs::read_to_string(java_home.join("release")).unwrap_or_default();
    if value(&release, "JAVA_RUNTIME_VERSION").is_none_or(str::is_empty) {
        return String::new();
    }
    let values = ["IMPLEMENTOR", "JAVA_RUNTIME_VERSION", "OS_ARCH"].map(|key| value(&release, key).unwrap_or(""));
    values.join(" ").chars().take(256).collect()
}

/// The value of `key` in a `release` file, without its quotes.
fn value<'a>(release: &'a str, key: &str) -> Option<&'a str> {
    let value = release.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix('='))?;
    Some(value.trim().trim_matches('"'))
}

/// The major version in a `JAVA_VERSION` value such as `25`, `21.0.4` or `1.8.0_392`.
fn parse_major(value: &str) -> Option<u32> {
    let value = value.strip_prefix("1.").unwrap_or(value);
    let digits = value.find(|c: char| !c.is_ascii_digit()).unwrap_or(value.len());
    value[..digits].parse().ok()
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
        for (value, major) in [("25", Some(25)), ("21.0.4", Some(21)), ("1.8.0_392", Some(8))] {
            assert_eq!(parse_major(value), major, "{value}");
        }
        assert_eq!(parse_major("26-ea"), Some(26));
        assert_eq!(parse_major(""), None);
    }

    #[test]
    fn the_heap_is_the_visible_or_profile_memory_less_the_reserve() {
        assert_eq!(heap_mib(Some(1024), None).unwrap(), 1024 - 200 - 102);
        assert_eq!(heap_mib(Some(1024), Some(512)).unwrap(), 512 - 200 - 51);
        assert_eq!(heap_mib(None, Some(4096)).unwrap(), 4096 - 200 - 409);
        assert_eq!(heap_mib(None, None).unwrap_err().code, 64);
        assert_eq!(heap_mib(Some(256), None).unwrap_err().code, 64);
    }
}
