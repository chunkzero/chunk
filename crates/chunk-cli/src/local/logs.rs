use tracing_subscriber::EnvFilter;

/// Service logs stay at warnings in plain output; `RUST_LOG` restores detail.
pub(super) fn plain() {
    tracing_subscriber::fmt().with_env_filter(filter("warn")).init();
}

fn filter(default: &str) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| default.into())
}
