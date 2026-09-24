use std::{collections::VecDeque, sync::Arc, time::Duration};

use anyhow::{Result, ensure};
use tokio::{
    task::JoinSet,
    time::{Instant, sleep_until, timeout_at},
};

use crate::{config::Config, control, metrics::Stats, proxy};

pub enum Client {
    Proxy(proxy::Client),
    Control(control::Client),
}

impl Client {
    async fn execute(&mut self, sequence: u64, config: &Config) -> Result<()> {
        match self {
            Self::Proxy(client) => client.exchange(sequence).await,
            Self::Control(client) => client.execute(sequence, config).await,
        }
    }
}

struct Finished {
    client: Option<Client>,
    latency: Duration,
    error: Option<String>,
}

fn collect(finished: Finished, clients: &mut VecDeque<Client>, stats: &mut Stats) -> Result<()> {
    if let Some(client) = finished.client {
        clients.push_back(client);
    }
    stats.observe(finished.latency, finished.error)
}

pub async fn run(
    config: Arc<Config>,
    clients: &mut VecDeque<Client>,
    seconds: u32,
    sequence_offset: u64,
) -> Result<Stats> {
    let offers = u64::from(seconds) * u64::from(config.rate());
    let mut stats = Stats::new(offers)?;
    let start = Instant::now();
    let end = start + Duration::from_secs(u64::from(seconds));
    let mut tasks = JoinSet::new();
    for index in 0..offers {
        let scheduled = start + Duration::from_nanos(index * 1_000_000_000 / u64::from(config.rate()));
        loop {
            tokio::select! {
                biased;
                Some(result) = tasks.join_next(), if !tasks.is_empty() => collect(result?, clients, &mut stats)?,
                () = sleep_until(scheduled) => break,
            }
        }
        let now = Instant::now();
        if now >= end {
            stats.dropped_late += offers - index;
            break;
        }
        stats.scheduled(now.saturating_duration_since(scheduled))?;
        let deadline = scheduled + Duration::from_millis(u64::from(config.timeout_ms));
        if now >= deadline {
            stats.dropped_late += 1;
            continue;
        }
        let Some(mut client) = clients.pop_front() else {
            stats.dropped_busy += 1;
            continue;
        };
        let config = config.clone();
        tasks.spawn(async move {
            let error = match timeout_at(deadline, client.execute(index + sequence_offset, &config)).await {
                Err(_) => Some("timeout".into()),
                Ok(Err(error)) => Some(error.downcast_ref::<tonic::Status>().map_or_else(
                    || format!("workload: {error}"),
                    |status| format!("grpc/{:?}: {}", status.code(), status.message()),
                )),
                Ok(Ok(())) => None,
            };
            // A canceled or failed frame exchange cannot safely reuse the cipher/framing state.
            let reusable = error.is_none() || matches!(client, Client::Control(_));
            Finished { client: reusable.then_some(client), latency: Instant::now().duration_since(scheduled), error }
        });
    }
    sleep_until(end).await;
    while let Some(result) = tasks.join_next().await {
        collect(result?, clients, &mut stats)?;
    }
    stats.elapsed = start.elapsed();
    ensure!(
        stats.offered == stats.completed + stats.errors.values().sum::<u64>() + stats.dropped_busy + stats.dropped_late,
        "load accounting mismatch"
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[tokio::test(start_paused = true)]
    async fn saturated_generator_keeps_offering_instead_of_waiting_for_capacity() {
        let config = Config::parse_from(["bench", "proxy-relay", "--seconds", "2", "--rate", "100"]);
        let stats = run(Arc::new(config), &mut VecDeque::new(), 2, 0).await.unwrap();
        assert_eq!(stats.offered, 200);
        assert_eq!(stats.dropped_busy, 200);
        assert_eq!(stats.completed, 0);
    }
}
