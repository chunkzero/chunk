//! Host and process resource usage sampled on demand for the dashboard.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use serde::Serialize;
use sysinfo::{ProcessesToUpdate, System};

#[derive(Clone, Serialize, utoipa::ToSchema)]
pub struct Sample {
    pub hostname: Option<String>,
    pub os: Option<String>,
    pub cpus: usize,
    /// Whole-machine CPU usage in percent since the previous sample.
    pub cpu_percent: f32,
    pub load_average: [f64; 3],
    pub memory_total: u64,
    pub memory_used: u64,
    pub memory_available: u64,
    pub swap_total: u64,
    pub swap_used: u64,
    pub process_memory: u64,
    pub process_cpu_percent: f32,
}

/// Keeps one `sysinfo` state so CPU percentages are deltas between calls.
pub struct Machine {
    system: Mutex<System>,
    cached: Mutex<Option<(Instant, Sample)>>,
}

impl Default for Machine {
    fn default() -> Self {
        let mut system = System::new();
        Self::refresh(&mut system);
        Self {
            system: Mutex::new(system),
            cached: Mutex::new(None),
        }
    }
}

impl Machine {
    fn refresh(system: &mut System) {
        system.refresh_memory();
        system.refresh_cpu_usage();
        if let Ok(pid) = sysinfo::get_current_pid() {
            system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        }
    }

    #[must_use]
    pub fn sample(&self) -> Sample {
        let mut cached = self.cached.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((at, sample)) = cached.as_ref()
            && at.elapsed() < Duration::from_secs(2)
        {
            return sample.clone();
        }
        let mut system = self.system.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::refresh(&mut system);
        let process = sysinfo::get_current_pid().ok().and_then(|pid| system.process(pid));
        let load = System::load_average();
        let sample = Sample {
            hostname: System::host_name(),
            os: System::long_os_version(),
            cpus: system.cpus().len(),
            cpu_percent: system.global_cpu_usage(),
            load_average: [load.one, load.five, load.fifteen],
            memory_total: system.total_memory(),
            memory_used: system.used_memory(),
            memory_available: system.available_memory(),
            swap_total: system.total_swap(),
            swap_used: system.used_swap(),
            process_memory: process.map_or(0, sysinfo::Process::memory),
            process_cpu_percent: process.map_or(0.0, sysinfo::Process::cpu_usage),
        };
        *cached = Some((Instant::now(), sample.clone()));
        sample
    }
}
