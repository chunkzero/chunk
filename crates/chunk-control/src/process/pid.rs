//! The PID of each spawned JVM, with its start time, so a JVM re-attached after control restarted can still be killed,
//! and a later process reusing its PID never is. Start times come from `/proc` and the kill goes through a pidfd, so
//! only Linux kills a re-attached JVM; elsewhere, or on a kernel without pidfds, none is killed.

use crate::{Error, Result};
use std::{io::Write, path::Path};

#[derive(serde::Serialize, serde::Deserialize)]
struct Spawned {
    pid: u32,
    /// Clock ticks from boot to the process's start.
    started: u64,
}

/// Records at `path` that the JVM spawned as `pid`.
pub(super) fn record(path: &Path, pid: u32) -> Result<()> {
    let spawned = Spawned { pid, started: started(pid)? };
    let mut file = chunk_service::private_file(path)?;
    file.write_all(&serde_json::to_vec(&spawned)?)?;
    file.sync_all()?;
    Ok(())
}

/// Kills the JVM recorded at `path`, once its PID is confirmed to still name the process spawned then. The pidfd is
/// opened before that check, so a process that reuses the PID afterwards is never signalled.
#[cfg(target_os = "linux")]
pub(super) fn kill(path: &Path) -> Result<()> {
    use rustix::process::{Pid, PidfdFlags, Signal, pidfd_open, pidfd_send_signal};
    let spawned: Spawned = serde_json::from_slice(&std::fs::read(path)?)?;
    let pid = i32::try_from(spawned.pid).ok().and_then(Pid::from_raw).ok_or(Error::Invalid("recorded JVM PID"))?;
    let pidfd = pidfd_open(pid, PidfdFlags::empty()).map_err(std::io::Error::from)?;
    if started(spawned.pid)? != spawned.started {
        return Err(Error::Unresolved("the JVM's PID names another process"));
    }
    pidfd_send_signal(&pidfd, Signal::KILL).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn kill(_path: &Path) -> Result<()> {
    Err(Error::Unresolved("killing a re-attached JVM requires Linux"))
}

/// When `pid` started, from `/proc/<pid>/stat`.
fn started(pid: u32) -> Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The command name, in parentheses, may hold spaces; the start time is the 20th field after it.
    let started = stat.rsplit_once(')').and_then(|(_, fields)| fields.split_whitespace().nth(19)?.parse().ok());
    started.ok_or(Error::Unresolved("unreadable process start time"))
}
