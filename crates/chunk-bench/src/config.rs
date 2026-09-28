use anyhow::{Result, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    ProxyRelay,
    ControlPopulation,
    ControlChurn,
    BackendQuery,
    BackendMutation,
    SyncQueries,
}

impl Scenario {
    /// Workloads served from the compiled backend bundle.
    pub fn is_backend(self) -> bool {
        matches!(self, Self::BackendQuery | Self::BackendMutation | Self::SyncQueries)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Writes {
    /// Commits to a table no subscribed query reads, so streams only advance their position.
    Unrelated,
    /// Sets a new leaderboard record, changing every subscribed result.
    Related,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Payload {
    Repeated,
    Mixed,
    Random,
    /// A synthetic overworld chunk column (about 50 KiB); replaces `--response-bytes` downstream.
    Chunk,
}

#[derive(Clone, Debug, Parser, Serialize, Deserialize)]
#[command(about = "Local, opt-in service workloads; results under target/bench")]
pub struct Config {
    pub scenario: Scenario,
    /// Measured offered-load window, excluding setup, warmup and drain.
    #[arg(long, default_value_t = 10)]
    pub seconds: u32,
    #[arg(long, default_value_t = 2)]
    pub warmup: u32,
    /// Total operations/second. Defaults: relay 2000, population 100, churn 10, backend query 1000,
    /// mutation 100, sync queries 100.
    #[arg(long)]
    pub rate: Option<u32>,
    /// Established proxy connections or independent control/backend RPC lanes.
    #[arg(long, default_value_t = 64)]
    pub concurrency: u32,
    /// Arrived players (control) or player profiles (backend) seeded before the workload.
    #[arg(long, default_value_t = 128)]
    pub population: u32,
    /// Sync queries: query subscriptions held open while writes run.
    #[arg(long, default_value_t = 5000)]
    pub subscribers: u32,
    /// Sync queries: what each write changes.
    #[arg(long, value_enum, default_value_t = Writes::Unrelated)]
    pub writes: Writes,
    /// Sync queries: write with the subscribers' credential, so every stream catches up to each write promptly.
    #[arg(long)]
    pub own_writes: bool,
    /// Sync queries: subscription streams multiplexed on each connection.
    #[arg(long, default_value_t = 100)]
    pub streams_per_connection: u32,
    /// Sync queries: streams that wait `--slow-read-ms` before each read, on their own connections; excluded from lag.
    #[arg(long, default_value_t = 0)]
    pub slow_readers: u32,
    #[arg(long, default_value_t = 5000)]
    pub slow_read_ms: u32,
    /// Sync queries: bytes of padding added to the leaderboard result, e.g. 65536, to press on core's send budget.
    #[arg(long, default_value_t = 0)]
    pub result_padding: u32,
    /// Pin the target process with `taskset -c`, e.g. `12-13`. Pin the generator by running under taskset.
    #[arg(long)]
    pub target_cpus: Option<String>,
    /// Target Tokio worker threads. Defaults to the available parallelism, as `chunk-environment`'s `#[tokio::main]`.
    #[arg(long, default_value_t = available_parallelism())]
    pub target_threads: usize,
    #[arg(long, default_value_t = 2)]
    pub generator_threads: usize,
    /// Deadline from the scheduled send time, including generator delay.
    #[arg(long, default_value_t = 5000)]
    pub timeout_ms: u32,
    #[arg(long, default_value_t = 32)]
    pub request_bytes: usize,
    #[arg(long, default_value_t = 1024)]
    pub response_bytes: usize,
    /// Relay response packets the synthetic gameplay server sends per request, in one write.
    #[arg(long, default_value_t = 1)]
    pub burst: usize,
    #[arg(long, value_enum, default_value_t = Payload::Mixed)]
    pub payload: Payload,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    #[arg(long)]
    pub no_encryption: bool,
    #[arg(long)]
    pub no_compression: bool,
    #[arg(long, default_value_t = 256)]
    pub compression_threshold: usize,
    /// libdeflate level (1..=12) for the target and generator; defaults to the production level.
    #[arg(long)]
    pub compression_level: Option<i32>,
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

impl Config {
    pub fn rate(&self) -> u32 {
        self.rate.unwrap_or(match self.scenario {
            Scenario::ProxyRelay => 2000,
            Scenario::ControlChurn => 10,
            Scenario::BackendQuery => 1000,
            Scenario::ControlPopulation | Scenario::BackendMutation | Scenario::SyncQueries => 100,
        })
    }

    pub fn compression(&self) -> Option<usize> {
        (!self.no_compression).then_some(self.compression_threshold)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!((1..=3600).contains(&self.seconds) && self.warmup <= 60, "invalid duration");
        ensure!((1..=4096).contains(&self.concurrency), "concurrency must be 1..=4096");
        let population = if self.scenario.is_backend() { 100_000 } else { 1024 };
        ensure!((1..=population).contains(&self.population), "population must be 1..={population}");
        ensure!((1..=100_000).contains(&self.subscribers), "subscribers must be 1..=100000");
        ensure!((1..=10_000).contains(&self.streams_per_connection), "streams per connection must be 1..=10000");
        ensure!(
            self.slow_readers <= self.subscribers && (1..=600_000).contains(&self.slow_read_ms),
            "slow readers must be at most the subscribers, and their read delay 1..=600000 ms"
        );
        ensure!(self.result_padding <= 512 * 1024, "result padding must be at most 524288 bytes");
        ensure!(
            self.target_cpus.as_ref().is_none_or(|cpus| {
                !cpus.is_empty() && cpus.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '-')
            }),
            "target CPUs must be a taskset list such as 12-13"
        );
        ensure!(
            (1..=1024).contains(&self.target_threads) && (1..=64).contains(&self.generator_threads),
            "invalid thread count"
        );
        ensure!((1..=60_000).contains(&self.timeout_ms), "timeout must be 1..=60000 ms");
        ensure!((9..=65_536).contains(&self.request_bytes), "request bytes must be 9..=65536");
        ensure!((9..=2_000_000).contains(&self.response_bytes), "response bytes must be 9..=2000000");
        ensure!((1..=64).contains(&self.burst), "burst must be 1..=64");
        ensure!(self.compression_threshold <= 2_000_000, "invalid compression threshold");
        ensure!(
            self.compression_level.is_none_or(|level| (1..=12).contains(&level)),
            "compression level must be 1..=12"
        );
        let offers = u64::from(self.rate()) * u64::from(self.seconds + self.warmup);
        ensure!(self.rate() > 0 && offers <= 10_000_000, "rate must be positive; at most 10 million offers/run");
        if self.scenario == Scenario::ControlChurn {
            ensure!(
                self.population + self.concurrency <= 1024,
                "control holds at most 1024 open claims: population + concurrency must be <= 1024"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn churn_budget_includes_in_flight_and_seeded_players() {
        let mut config = Config::parse_from(["bench", "control-churn", "--population", "960", "--concurrency", "64"]);
        assert!(config.validate().is_ok());
        config.population += 1;
        assert!(config.validate().unwrap_err().to_string().contains("1024"));
        config.scenario = Scenario::ControlPopulation;
        assert!(config.validate().is_ok());
    }
}
