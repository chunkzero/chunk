//! Admission limits. New work is refused once queued work of its kind waits too long
//! or a memory budget runs out, and each refusal names its limit.
use std::{
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::{Duration, Instant},
};

use crate::{Error, Result};

/// Longest wait queued work may reach before new work of the same kind is refused.
pub(crate) const QUEUE_WAIT: Duration = Duration::from_millis(500);
/// Admitted requests, including replies waiting for durability, charge their input plus this.
pub(crate) const REQUEST_OVERHEAD: usize = 1024;
pub(crate) const REQUEST_BYTES: usize = 64 * 1024 * 1024;
/// Subscribed calls and their latest results.
pub(crate) const SUBSCRIPTION_BYTES: usize = 256 * 1024 * 1024;
/// Admitted mutations plus staged writes, results and replaced values.
pub(crate) const MUTATION_BYTES: usize = 32 * 1024 * 1024;
/// Each live action reserves its engine's heap limit. The budget is [`action_bytes`], never less than this.
pub(crate) const ACTION_BYTES: usize = 256 * 1024 * 1024;
/// Live actions may reserve 1/`ACTION_SHARE` of the machine's memory. Core may share the machine with Minecraft servers,
/// and each engine may also hold `ArrayBuffer` backing storage up to its heap limit beyond its reservation.
const ACTION_SHARE: usize = 8;
/// Retained action outcomes, live actions and prepared action identities; outcomes give way first.
pub(crate) const RETAINED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Limit {
    #[error("requests waited over 500 ms for the engine thread")]
    EngineQueue,
    #[error("queries waited over 500 ms for a read engine")]
    ReadQueue,
    #[error("mutations waited over 500 ms to commit")]
    CommitQueue,
    #[error("admitted requests reached 64 MiB")]
    RequestMemory,
    #[error("pending mutations reached 32 MiB")]
    MutationMemory,
    #[error("subscriptions reached 256 MiB")]
    SubscriptionMemory,
    #[error("live actions reached their engine heap budget and queued actions filled the queue or waited over 500 ms")]
    ActionMemory,
    #[error("live actions and prepared action identities reached 64 MiB of retention")]
    Retention,
    #[error("scheduled jobs reached the store's job budget; retry once jobs finish or expire")]
    Jobs,
}

impl Limit {
    pub(crate) fn exceeded(self) -> Error {
        Error::Overloaded(self)
    }
}

/// The live-action budget: [`ACTION_SHARE`] of the machine's memory, or of the process's cgroup limit when lower, and
/// [`ACTION_BYTES`] when either can't be established.
pub(crate) fn action_bytes() -> usize {
    machine_memory(|path| std::fs::read_to_string(path))
        .map_or(ACTION_BYTES, |bytes| (bytes / ACTION_SHARE).max(ACTION_BYTES))
}

/// The machine's memory bounded by the process's cgroup limit, given a reader for `/proc` and the cgroup files.
pub(crate) fn machine_memory(read: impl Fn(&Path) -> io::Result<String>) -> Option<usize> {
    let proc = |path: &str| read(Path::new(path)).ok();
    let meminfo = proc("/proc/meminfo")?;
    let total = meminfo.lines().find_map(|line| line.strip_prefix("MemTotal:"))?.trim().strip_suffix(" kB")?;
    let total = total.parse::<usize>().ok()?.checked_mul(1024)?;
    let limit = cgroup_limit(&proc("/proc/self/mountinfo")?, &proc("/proc/self/cgroup")?, &read)?;
    Some(limit.min(total))
}

/// The lowest memory limit on the process's cgroup or any ancestor visible through its memory controller's mounts,
/// given `/proc/self/mountinfo`, `/proc/self/cgroup` and a reader for the mounted files. The controller is the v1
/// memory hierarchy when the process has one, and otherwise the v2 hierarchy. `usize::MAX` means no visible level
/// limits memory; `None` means the controller, the process's group or a limit couldn't be resolved.
fn cgroup_limit(mountinfo: &str, groups: &str, read: impl Fn(&Path) -> io::Result<String>) -> Option<usize> {
    let membership = |member: fn(&str) -> bool| {
        groups.lines().find_map(|line| {
            let mut fields = line.splitn(3, ':');
            let (_, controllers, path) = (fields.next()?, fields.next()?, fields.next()?);
            member(controllers).then_some(path)
        })
    };
    let v1_path = membership(|controllers| controllers.split(',').any(|name| name == "memory"));
    let v1 = v1_path.is_some();
    let path = v1_path.or_else(|| membership(str::is_empty));
    let mounts: Vec<_> = mountinfo
        .lines()
        .filter_map(|line| {
            // `id parent device root mount-point options [optional...] - type source super-options`
            let (mount, filesystem) = line.split_once(" - ")?;
            let mut mount = mount.split(' ').skip(3);
            let (root, point) = (unescape(mount.next()?), unescape(mount.next()?));
            let mut filesystem = filesystem.split(' ');
            let memory_v1 = match (filesystem.next()?, filesystem.nth(1)?) {
                ("cgroup2", _) => false,
                ("cgroup", options) if options.split(',').any(|option| option == "memory") => true,
                _ => return None,
            };
            Some((memory_v1, root, point))
        })
        .collect();
    let Some(path) = path else {
        return mounts.is_empty().then_some(usize::MAX);
    };
    let file = if v1 { "memory.limit_in_bytes" } else { "memory.max" };
    let (mut exposed, mut lowest) = (false, usize::MAX);
    for (_, root, point) in mounts.iter().filter(|(memory_v1, ..)| *memory_v1 == v1) {
        // The mount exposes the hierarchy below `root`; the process's group must lie within it.
        let relative = path.strip_prefix(root.trim_end_matches('/'));
        let Some(relative) = relative.filter(|relative| relative.is_empty() || relative.starts_with('/')) else {
            continue;
        };
        let mut directory = PathBuf::from(point);
        let mut limit = parse_limit(read(&directory.join(file)))?;
        for component in relative.split('/').filter(|component| !component.is_empty()) {
            if component == ".." {
                return None;
            }
            directory.push(component);
            limit = limit.min(parse_limit(read(&directory.join(file)))?);
        }
        if v1 {
            // Covers ancestors above the mount's root, which a subtree mount hides.
            let stat = read(&directory.join("memory.stat")).map(|stat| {
                stat.lines()
                    .find_map(|line| line.strip_prefix("hierarchical_memory_limit "))
                    .unwrap_or_default()
                    .to_owned()
            });
            limit = limit.min(parse_limit(stat)?);
        }
        (exposed, lowest) = (true, lowest.min(limit));
    }
    exposed.then_some(lowest)
}

/// Decodes the octal escapes mountinfo writes for spaces, tabs, newlines and backslashes in paths.
fn unescape(field: &str) -> String {
    field.replace("\\040", " ").replace("\\011", "\t").replace("\\012", "\n").replace("\\134", "\\")
}

/// A limit file's bytes; `usize::MAX` when it's unlimited or absent, and `None` when it can't be read or parsed.
fn parse_limit(file: io::Result<String>) -> Option<usize> {
    let text = match file {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Some(usize::MAX),
        file => file.ok()?,
    };
    match text.trim() {
        "max" => Some(usize::MAX),
        bytes => Some(usize::try_from(bytes.parse::<u64>().ok()?).unwrap_or(usize::MAX)),
    }
}

/// Wait for the engine thread, shared by request admission and the engine.
#[derive(Default)]
pub(crate) struct EngineQueue {
    queued: AtomicUsize,
    /// Nanoseconds the most recently dequeued request waited.
    wait: AtomicU64,
}

impl EngineQueue {
    pub fn enter(&self) -> Result<()> {
        let waited = Duration::from_nanos(self.wait.load(Ordering::Relaxed));
        if self.queued.load(Ordering::Relaxed) != 0 && waited > QUEUE_WAIT {
            return Err(Limit::EngineQueue.exceeded());
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Undoes `enter` for a request that never reached the queue.
    pub fn leave(&self) {
        self.queued.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn dequeued(&self, admitted: Instant) {
        let waited = u64::try_from(admitted.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.wait.store(waited, Ordering::Relaxed);
        self.leave();
    }
}
