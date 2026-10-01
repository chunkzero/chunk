use std::time::Duration;

use anyhow::{Result, ensure};
use clap::Parser;
use serde::Serialize;

/// Drives offline-mode Minecraft clients against a chunk gateway or edge and reports login and latency statistics.
#[derive(Clone, Debug, Parser, Serialize)]
pub struct Config {
    /// Where the bots connect, as `host:port`.
    #[arg(long)]
    pub address: String,
    /// The hostname the handshake names; the edge routes by it. Defaults to the host part of `--address`.
    #[arg(long)]
    pub hostname: Option<String>,
    /// Bots to start.
    #[arg(long, default_value_t = 100)]
    pub bots: u32,
    /// Index of the first bot, so several hosts can run disjoint names.
    #[arg(long, default_value_t = 0)]
    pub first: u32,
    /// Username prefix; each bot is named `<prefix><index>`, at most 16 characters.
    #[arg(long, default_value = "Bot")]
    pub prefix: String,
    /// New connections per second during the ramp.
    #[arg(long, default_value_t = 20.0)]
    pub login_rate: f64,
    /// Connections from this process still logging in at once; the edge refuses more than 32 per client address.
    #[arg(long, default_value_t = 32)]
    pub max_pending: u32,
    /// Position updates per second per bot in play; 0 stands still.
    #[arg(long, default_value_t = 20)]
    pub move_hz: u32,
    /// Seconds between play-state pings per bot, for round-trip latency; 0 disables them.
    #[arg(long, default_value_t = 5.0)]
    pub ping_interval: f64,
    /// A chat command each bot sends periodically, without the slash, such as `coin`.
    #[arg(long)]
    pub command: Option<String>,
    /// Seconds between each bot's chat commands.
    #[arg(long, default_value_t = 30.0)]
    pub command_interval: f64,
    /// View distance each bot reports.
    #[arg(long, default_value_t = 8)]
    pub view_distance: i8,
    /// Seconds from a bot's connect until it must be in play.
    #[arg(long, default_value_t = 90)]
    pub login_timeout: u64,
    /// Seconds to hold every bot after the ramp; 0 runs until Ctrl-C.
    #[arg(long, default_value_t = 60)]
    pub hold: u64,
    /// Seconds between progress lines on stderr.
    #[arg(long, default_value_t = 10)]
    pub stats_interval: u64,
    /// Print the final summary as JSON on stdout.
    #[arg(long)]
    pub json: bool,
    /// Tokio worker threads; defaults to the available parallelism.
    #[arg(long)]
    pub threads: Option<usize>,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.bots > 0 && self.bots <= 100_000, "bots must be 1..=100000");
        let longest = self.prefix.len() + (self.first + self.bots - 1).to_string().len();
        ensure!(
            !self.prefix.is_empty()
                && longest <= 16
                && self.prefix.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
            "prefix must be alphanumeric or '_', and names at most 16 characters"
        );
        ensure!(self.login_rate > 0.0 && self.login_rate <= 10_000.0, "login rate must be in (0, 10000]");
        ensure!(self.max_pending > 0, "max pending must be positive");
        ensure!(self.move_hz <= 20, "move rate must be at most 20 Hz, the client tick rate");
        ensure!(self.ping_interval >= 0.0 && self.command_interval > 0.0, "invalid ping or command interval");
        ensure!(self.command.as_ref().is_none_or(|command| command.len() < 200), "command too long");
        ensure!(self.view_distance >= 2, "view distance must be at least 2");
        ensure!(self.stats_interval > 0 && self.login_timeout > 0, "intervals must be positive");
        Ok(())
    }

    pub fn hostname(&self) -> &str {
        self.hostname.as_deref().unwrap_or_else(|| self.address.rsplit_once(':').map_or("", |(host, _)| host))
    }

    pub fn name(&self, index: u32) -> String {
        format!("{}{}", self.prefix, self.first + index)
    }

    pub fn ping_period(&self) -> Option<Duration> {
        (self.ping_interval > 0.0).then(|| Duration::from_secs_f64(self.ping_interval))
    }
}
