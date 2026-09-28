use std::{path::Path, process::Command, time::Duration};

use anyhow::{Context, Result};
use serde_json::{Value, json};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio_util::sync::CancellationToken;

use crate::{metrics::count, target::Target};

pub fn metadata(root: &Path) -> Result<Value> {
    let mut system = System::new();
    system.refresh_cpu_all();
    system.refresh_memory();
    let command = |program: &str, args: &[&str]| -> Result<String> {
        let output = Command::new(program).args(args).current_dir(root).output()?;
        anyhow::ensure!(output.status.success(), "metadata command failed: {program}");
        Ok(String::from_utf8(output.stdout)?.trim().into())
    };
    Ok(json!({
        "commit": command("git", &["rev-parse", "HEAD"] )?,
        "dirty": !command("git", &["status", "--porcelain"] )?.is_empty(),
        "rustc": command("rustc", &["--version"] )?,
        "release": !cfg!(debug_assertions),
        "os": System::long_os_version(), "architecture": std::env::consts::ARCH,
        "cpu": system.cpus().first().map(sysinfo::Cpu::brand), "logical_cpus": system.cpus().len(),
        "memory_bytes": system.total_memory(),
        "scope": "local loopback; target process isolated from generator and synthetic gameplay service, with synthetic JVMs in the target; no quotas; CPU affinity only as given by target_cpus and the caller"
    }))
}

pub struct Sampler {
    system: System,
    pids: [Pid; 2],
    initial_cpu_ms: [u64; 2],
    start: std::time::Instant,
}

impl Sampler {
    pub fn new(target_pid: u32) -> Result<Self> {
        let mut sampler = Self {
            system: System::new(),
            pids: [Pid::from_u32(target_pid), Pid::from_u32(std::process::id())],
            initial_cpu_ms: [0; 2],
            start: std::time::Instant::now(),
        };
        sampler.refresh();
        for (index, pid) in sampler.pids.iter().enumerate() {
            sampler.initial_cpu_ms[index] =
                sampler.system.process(*pid).context("benchmark process missing")?.accumulated_cpu_time();
        }
        Ok(sampler)
    }

    fn refresh(&mut self) {
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&self.pids),
            true,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );
    }

    async fn sample(&mut self, target: &mut Target) -> Result<Value> {
        self.refresh();
        let elapsed = self.start.elapsed().as_secs_f64();
        let mut sample = json!({"elapsed_seconds": elapsed});
        for (index, name) in ["target", "generator_and_fixtures"].into_iter().enumerate() {
            let process =
                self.system.process(self.pids[index]).context("benchmark process exited during measurement")?;
            let cpu_ms = process.accumulated_cpu_time().saturating_sub(self.initial_cpu_ms[index]);
            sample[name] = json!({"rss_bytes": process.memory(), "cpu_ms": cpu_ms,
                "average_cpu_cores": count(cpu_ms) / 1000.0 / elapsed});
        }
        if let Some(bytes) = target.send().await?.get("bytes") {
            sample["target"]["send_charged_bytes"] = bytes.clone();
        }
        Ok(sample)
    }

    /// Samples each second until `stop`, asking `target` for core's charged send bytes.
    pub async fn run(mut self, stop: CancellationToken, target: &mut Target) -> Result<Vec<Value>> {
        let mut samples = Vec::new();
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            tokio::select! {
                () = stop.cancelled() => {
                    samples.push(self.sample(target).await?);
                    break;
                }
                _ = interval.tick() => samples.push(self.sample(target).await?),
            }
        }
        Ok(samples)
    }
}
