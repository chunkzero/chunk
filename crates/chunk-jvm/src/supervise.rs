//! Runs the JVM as a child: forwards signals to it, kills it once it outlives the stop grace, and reaps orphans when
//! the runner is PID 1.

use crate::Failure;
use rustix::{
    io::Errno,
    process::{Pid, Signal, WaitOptions, WaitStatus, getpid, kill_process, wait, waitpid},
};
use std::{process::Command, time::Duration};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::mpsc,
    time::{Instant, interval, sleep_until},
};

/// Starts `command` and returns its exit code, or 128 plus the number of the signal that ended it. Each signal
/// received is forwarded to it; after the first SIGTERM or SIGINT it has `grace` to exit before it gets SIGKILL. A JVM
/// that a forwarded SIGTERM or SIGINT ends within that grace stopped as asked, so the runner reports 0.
pub(crate) async fn run(
    command: &mut Command,
    signals: &mut mpsc::UnboundedReceiver<Signal>,
    grace: Duration,
) -> Result<i32, Failure> {
    let failed = |error: std::io::Error| Failure::io(format!("cannot run the JVM: {error}"));
    // Registered before the spawn, so no exit goes unnoticed.
    let mut exits = signal(SignalKind::child()).map_err(failed)?;
    let child = command.spawn().map_err(|error| Failure::java(format!("cannot start Java: {error}")))?;
    let pid = Pid::from_child(&child);
    let orphans = getpid().is_init();
    // The occasional poll also covers exits whose SIGCHLD coalesced with an earlier one.
    let mut poll = interval(Duration::from_secs(1));
    let mut kill_at = None;
    let mut stops = Vec::new();
    loop {
        if let Some(status) = reap(pid, orphans).map_err(|error| failed(error.into()))? {
            let code = code(status);
            return Ok(if stops.iter().any(|stop: &Signal| code == 128 + stop.as_raw()) { 0 } else { code });
        }
        tokio::select! {
            _ = exits.recv() => {}
            _ = poll.tick() => {}
            Some(signal) = signals.recv() => {
                tracing::info!(signal = signal.as_raw(), "forwarding a signal to the JVM");
                send(pid, signal).map_err(|error| failed(error.into()))?;
                if signal != Signal::QUIT {
                    if stops.is_empty() {
                        kill_at = Some(Instant::now() + grace);
                    }
                    stops.push(signal);
                }
            }
            () = async { sleep_until(kill_at.unwrap_or_else(Instant::now)).await }, if kill_at.is_some() => {
                tracing::warn!(?grace, "the JVM outlived its stop grace; killing it");
                send(pid, Signal::KILL).map_err(|error| failed(error.into()))?;
                kill_at = None;
            }
        }
    }
}

/// The JVM's status once it exited, reaping any other exited children when `orphans` is set.
fn reap(pid: Pid, orphans: bool) -> rustix::io::Result<Option<WaitStatus>> {
    loop {
        let reaped = if orphans { wait(WaitOptions::NOHANG) } else { waitpid(Some(pid), WaitOptions::NOHANG) };
        match reaped {
            Ok(Some((reaped, status))) if reaped == pid => return Ok(Some(status)),
            Ok(Some(_)) | Err(Errno::INTR) => {}
            Ok(None) => return Ok(None),
            Err(error) => return Err(error),
        }
    }
}

/// Sends `signal` to `pid`, which may already have exited.
fn send(pid: Pid, signal: Signal) -> rustix::io::Result<()> {
    match kill_process(pid, signal) {
        Err(Errno::SRCH) => Ok(()),
        result => result,
    }
}

fn code(status: WaitStatus) -> i32 {
    status.exit_status().or_else(|| status.terminating_signal().map(|signal| 128 + signal)).unwrap_or(1)
}

#[cfg(test)]
mod tests;
