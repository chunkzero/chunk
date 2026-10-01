//! Load bots: `chunk-bots --address edge.example:25565 --bots 1000`. See `README.md`.
mod bot;
mod config;
mod packets;
mod pings;
mod stats;
mod wire;

use std::{
    sync::{Arc, atomic::Ordering::Relaxed},
    time::Duration,
};

use anyhow::Result;
use clap::Parser;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::{
    sync::Semaphore,
    task::JoinSet,
    time::{Instant, MissedTickBehavior, interval, sleep},
};
use tokio_util::sync::CancellationToken;

use config::Config;
use stats::Stats;

fn main() -> Result<()> {
    let config = Config::parse();
    config.validate()?;
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    if let Some(threads) = config.threads {
        runtime.worker_threads(threads);
    }
    runtime.enable_all().build()?.block_on(run(Arc::new(config)))
}

/// This process's CPU time since it started, in seconds.
struct Cpu {
    system: System,
    pid: Pid,
}

impl Cpu {
    fn seconds(&mut self) -> f64 {
        let pids = [self.pid];
        let refresh = ProcessRefreshKind::nothing().with_cpu();
        self.system.refresh_processes_specifics(ProcessesToUpdate::Some(&pids), true, refresh);
        #[allow(clippy::cast_precision_loss)]
        self.system.process(self.pid).map_or(0.0, |process| process.accumulated_cpu_time() as f64 / 1000.0)
    }
}

async fn run(config: Arc<Config>) -> Result<()> {
    let stats = Arc::new(Stats::new());
    let stop = CancellationToken::new();
    let mut cpu = Cpu { system: System::new(), pid: Pid::from_u32(std::process::id()) };
    let started = Instant::now();
    let initial_cpu = cpu.seconds();
    let progress = tokio::spawn(progress(config.clone(), stats.clone(), started));
    let mut bots = JoinSet::new();
    let pending = Arc::new(Semaphore::new(usize::try_from(config.max_pending)?));
    let ramp = async {
        let mut logins = interval(Duration::from_secs_f64(1.0 / config.login_rate));
        logins.set_missed_tick_behavior(MissedTickBehavior::Delay);
        for index in 0..config.bots {
            logins.tick().await;
            let permit = pending.clone().acquire_owned().await?;
            bots.spawn(bot::run(index, config.clone(), stats.clone(), permit, stop.clone()));
        }
        anyhow::Ok(())
    };
    let interrupted = tokio::select! {
        result = ramp => { result?; false }
        _ = tokio::signal::ctrl_c() => true,
    };
    if !interrupted {
        eprintln!("ramp complete after {:.0}s; holding", started.elapsed().as_secs_f64());
        let hold = async {
            if config.hold == 0 {
                std::future::pending::<()>().await;
            }
            sleep(Duration::from_secs(config.hold)).await;
        };
        tokio::select! {
            () = hold => {}
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    let elapsed = started.elapsed();
    let summary = stats.summary(elapsed, cpu.seconds() - initial_cpu);
    stop.cancel();
    progress.abort();
    while bots.join_next().await.is_some() {}
    if config.json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({"config": &*config, "summary": summary}))?);
    } else {
        eprintln!("{summary:#?}");
    }
    Ok(())
}

async fn progress(config: Arc<Config>, stats: Arc<Stats>, started: Instant) {
    let mut cpu = Cpu { system: System::new(), pid: Pid::from_u32(std::process::id()) };
    let period = Duration::from_secs(config.stats_interval);
    let mut ticks = interval(period);
    ticks.tick().await;
    let (mut last_cpu, mut last_bytes) = (cpu.seconds(), 0);
    loop {
        ticks.tick().await;
        let (now_cpu, bytes) = (cpu.seconds(), stats.bytes_in.load(Relaxed));
        #[allow(clippy::cast_precision_loss)]
        let rate = (bytes - last_bytes) as f64 / period.as_secs_f64();
        eprintln!("{}", stats.progress(started.elapsed(), rate, (now_cpu - last_cpu) / period.as_secs_f64()));
        (last_cpu, last_bytes) = (now_cpu, bytes);
    }
}
