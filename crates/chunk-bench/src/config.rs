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
    BackendFanout,
}

impl Scenario {
    pub fn is_backend(self) -> bool {
        matches!(self, Self::BackendQuery | Self::BackendMutation | Self::BackendFanout)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Subscription {
    /// Everyone watches the identical leaderboard query.
    Shared,
    /// Everyone watches their own standing: the leaderboard plus their profile.
    PerPlayer,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Payload {
    Repeated,
    Mixed,
    Random,
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
    /// Total operations/second. Defaults: relay 2000, population 2/player, churn 10, backend query 1000,
    /// mutation 100, fan-out 10.
    #[arg(long)]
    pub rate: Option<u32>,
    /// Established proxy connections or independent control/backend RPC lanes.
    #[arg(long, default_value_t = 64)]
    pub concurrency: u32,
    /// Arrived players (control) or player profiles (backend) seeded before the workload.
    #[arg(long, default_value_t = 128)]
    pub population: u32,
    /// Backend fan-out: query subscriptions held open while writes change the leaderboard.
    #[arg(long, default_value_t = 64)]
    pub subscribers: u32,
    /// Backend fan-out: subscriptions per watch-group stream; each stream has its own connection.
    #[arg(long, default_value_t = 1)]
    pub group_size: u32,
    #[arg(long, value_enum, default_value_t = Subscription::Shared)]
    pub subscription: Subscription,
    /// Pin the target process with `taskset -c`, e.g. `12-13`. Pin the generator by running under taskset.
    #[arg(long)]
    pub target_cpus: Option<String>,
    #[arg(long, default_value_t = 2)]
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
}

impl Config {
    pub fn rate(&self) -> u32 {
        self.rate.unwrap_or(match self.scenario {
            Scenario::ProxyRelay => 2000,
            Scenario::ControlPopulation => self.population * 2,
            Scenario::ControlChurn | Scenario::BackendFanout => 10,
            Scenario::BackendQuery => 1000,
            Scenario::BackendMutation => 100,
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
        ensure!(
            (1..=100_000).contains(&self.subscribers) && (1..=1024).contains(&self.group_size),
            "subscribers must be 1..=100000 and group size 1..=1024"
        );
        ensure!(
            self.target_cpus.as_ref().is_none_or(|cpus| {
                !cpus.is_empty() && cpus.chars().all(|c| c.is_ascii_digit() || c == ',' || c == '-')
            }),
            "target CPUs must be a taskset list such as 12-13"
        );
        ensure!(
            (1..=64).contains(&self.target_threads) && (1..=64).contains(&self.generator_threads),
            "invalid thread count"
        );
        ensure!((1..=60_000).contains(&self.timeout_ms), "timeout must be 1..=60000 ms");
        ensure!((9..=65_536).contains(&self.request_bytes), "request bytes must be 9..=65536");
        ensure!((9..=2_000_000).contains(&self.response_bytes), "response bytes must be 9..=2000000");
        ensure!(self.compression_threshold <= 2_000_000, "invalid compression threshold");
        let offers = u64::from(self.rate()) * u64::from(self.seconds + self.warmup);
        ensure!(self.rate() > 0 && offers <= 10_000_000, "rate must be positive; at most 10 million offers/run");
        if self.scenario == Scenario::ControlChurn {
            ensure!(
                offers + u64::from(self.population) <= 1024,
                "control retains at most 1024 claim/operation IDs: population + (seconds + warmup) * rate must be <= 1024; use shorter runs and fresh state"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn churn_budget_includes_warmup_and_seeded_players() {
        let mut config = Config::parse_from(["bench", "control-churn", "--population", "904", "--rate", "10"]);
        assert!(config.validate().is_ok());
        config.population += 1;
        assert!(config.validate().unwrap_err().to_string().contains("1024"));
        config.scenario = Scenario::ControlPopulation;
        assert!(config.validate().is_ok());
    }
}
