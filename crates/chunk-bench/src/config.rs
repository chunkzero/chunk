use anyhow::{Result, ensure};
use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Scenario {
    ProxyRelay,
    ControlPopulation,
    ControlChurn,
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
    /// Total operations/second. Defaults: relay 2000, population 2/player, churn 10.
    #[arg(long)]
    pub rate: Option<u32>,
    /// Established proxy connections or independent control RPC lanes.
    #[arg(long, default_value_t = 64)]
    pub concurrency: u32,
    /// Arrived players seeded before either control workload.
    #[arg(long, default_value_t = 128)]
    pub population: u32,
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
            Scenario::ControlChurn => 10,
        })
    }

    pub fn compression(&self) -> Option<usize> {
        (!self.no_compression).then_some(self.compression_threshold)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!((1..=3600).contains(&self.seconds) && self.warmup <= 60, "invalid duration");
        ensure!((1..=4096).contains(&self.concurrency), "concurrency must be 1..=4096");
        ensure!((1..=1024).contains(&self.population), "population must be 1..=1024");
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
