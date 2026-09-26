//! What the OS reports about a launched JVM, so a launch this host does not own can be confirmed to have exited.
//! A PID only ever proves that a launch ended; nothing here adopts or signals a process.
//!
//! On Linux, the boot ID and the start time in `/proc/<pid>/stat` (clock ticks after boot) identify a launch. Other
//! platforms have no boot ID here, so the start time `sysinfo` reports (seconds since the epoch) alone tells a reused
//! PID apart; a reboot ends every process, leaving the PID gone or owned by a process that started later.

use std::io;

use serde::{Deserialize, Serialize};

/// A spawned JVM as the OS knows it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Launched {
    pub pid: u32,
    /// The start time the OS reports for `pid`, in the platform's unit.
    pub started: u64,
}

/// Whether a JVM launched during `boot`, and spawned as `launched` if recorded, has certainly exited: it was launched
/// in another boot, its PID is gone, or the PID now belongs to a process with another start time. Unreadable or
/// unrecorded details never confirm an exit.
pub(super) fn exited(boot: Option<&str>, launched: Option<&Launched>) -> bool {
    match (boot, current_boot().as_deref()) {
        (Some(recorded), Some(current)) if recorded != current => return true,
        (recorded, current) if recorded != current => return false,
        _ => {}
    }
    launched.is_some_and(|launched| started(launched.pid).is_ok_and(|current| current != Some(launched.started)))
}

/// This boot's identity, where the platform has one.
#[cfg(target_os = "linux")]
pub(super) fn current_boot() -> Option<String> {
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(boot.trim().to_owned()).filter(|boot| !boot.is_empty())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn current_boot() -> Option<String> {
    None
}

/// The start time of the process with `pid`, or `None` when no process has it.
/// # Errors
/// Reports process details that cannot be read.
#[cfg(target_os = "linux")]
pub(super) fn started(pid: u32) -> io::Result<Option<u64>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    // The command name in field 2 may contain spaces, so fields are counted after its closing parenthesis.
    stat.rsplit_once(')')
        .and_then(|(_, fields)| fields.split_whitespace().nth(22 - 3))
        .and_then(|started| started.parse().ok())
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "unreadable process start time"))
}

#[cfg(not(target_os = "linux"))]
pub(super) fn started(pid: u32) -> io::Result<Option<u64>> {
    let pid = sysinfo::Pid::from_u32(pid);
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::Some(&[pid]), true);
    Ok(system.process(pid).map(sysinfo::Process::start_time))
}
