//! The PID of each spawned JVM, with its start time, so a JVM re-attached after control restarted can still be killed,
//! and a later process reusing its PID never is. Start times come from `/proc`; where it is missing, no JVM is killed.

use crate::{Error, Result};
use std::{io::Write, path::Path};
use tokio::process::Command;

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

/// Kills the JVM recorded at `path`, once its PID is confirmed to still name the process spawned then.
pub(super) async fn kill(path: &Path) -> Result<()> {
    let spawned: Spawned = serde_json::from_slice(&std::fs::read(path)?)?;
    if started(spawned.pid)? != spawned.started {
        return Err(Error::Unresolved("the JVM's PID names another process"));
    }
    if !Command::new("kill").arg("-KILL").arg(spawned.pid.to_string()).status().await?.success() {
        return Err(Error::Unresolved("the JVM could not be killed"));
    }
    Ok(())
}

/// When `pid` started, from `/proc/<pid>/stat`.
fn started(pid: u32) -> Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    // The command name, in parentheses, may hold spaces; the start time is the 20th field after it.
    let started = stat.rsplit_once(')').and_then(|(_, fields)| fields.split_whitespace().nth(19)?.parse().ok());
    started.ok_or(Error::Unresolved("unreadable process start time"))
}
