//! Backend workloads: the TypeScript bundle in `backend/`, compiled like an app and served by core's backend with
//! on-disk SQLite, whose functions the CLI calls over the sync protocol.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use chunk_backend::Phase;
use chunk_proto::sync::v1::{CallRequest, core_client::CoreClient};
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use serde_json::{Value, json};
use tonic::transport::Channel;

use crate::{
    config::{Config, Scenario},
    metrics, sync,
};

pub const ENVIRONMENT: &str = "bench";
pub const SEED_BATCH: u32 = 200;
const SOURCES: [(&str, &str); 4] = [
    ("server/schema/index.ts", include_str!("../backend/server/schema/index.ts")),
    ("server/players.ts", include_str!("../backend/server/players.ts")),
    ("server/leaderboard.ts", include_str!("../backend/server/leaderboard.ts")),
    ("server/activity.ts", include_str!("../backend/server/activity.ts")),
];

/// Compiles the bundle into `output/bundle` (including `source.mjs`) and returns the deployment path.
pub fn compile(output: &Path) -> Result<PathBuf> {
    let project = tempfile::tempdir()?;
    for (path, source) in SOURCES {
        let path = project.path().join(path);
        fs::create_dir_all(path.parent().context("source directory")?)?;
        fs::write(path, source)?;
    }
    let bundle = output.join("bundle");
    chunk_build::compile(project.path(), &bundle)
        .context("compiling the benchmark bundle; native TypeScript must be installed or set in CHUNK_TYPESCRIPT")?;
    let mut deployment: Value = serde_json::from_slice(&fs::read(bundle.join("contract.json"))?)?;
    deployment["id"] = "bench".into();
    deployment["source"] = fs::read_to_string(bundle.join("source.mjs"))?.into();
    let path = bundle.join("deployment.json");
    fs::write(&path, serde_json::to_vec(&deployment)?)?;
    Ok(path)
}

fn save(sequence: u64, player: &str) -> Value {
    let inventory: Vec<_> =
        (0..16).map(|slot| json!({"item": format!("item-{}", (sequence + slot) % 64), "count": 1 + slot})).collect();
    json!({"player": player, "coins": sequence % 10_000, "xp": sequence * 3, "level": 1 + sequence % 50,
        "best": sequence * 7 % 100_000, "inventory": inventory})
}

/// A lane calling the bundle's functions through core's sync `Call` with the CLI's credential.
pub struct Client {
    rpc: CoreClient<Channel>,
    cli: String,
}

impl Client {
    pub async fn connect(connection: &sync::Connection) -> Result<Self> {
        Ok(Self { rpc: sync::connect(&connection.endpoint).await?, cli: connection.cli.clone() })
    }

    pub async fn execute(&mut self, sequence: u64, config: &Config) -> Result<()> {
        let player = format!("p{}", sequence % u64::from(config.population));
        let (operation, function, arguments) = match config.scenario {
            Scenario::BackendQuery => (String::new(), "shared/players/load", json!({"player": player})),
            Scenario::BackendMutation => (format!("save-{sequence}"), "shared/players/save", save(sequence, &player)),
            _ => anyhow::bail!("not a backend workload"),
        };
        let message = CallRequest {
            operation_id: operation,
            method: function.into(),
            arguments: arguments.to_string().into_bytes(),
            deployment: sync::DEPLOYMENT.into(),
            ..CallRequest::default()
        };
        let (result, _) = sync::call(&mut self.rpc, &self.cli, message).await?;
        if config.scenario == Scenario::BackendQuery {
            ensure!(result.starts_with(b"{"), "profile missing");
        } else {
            ensure!(std::str::from_utf8(&result)?.parse::<u64>().is_ok(), "unexpected save result");
        }
        Ok(())
    }
}

static PHASES: Mutex<BTreeMap<Phase, Histogram<u64>>> = Mutex::new(BTreeMap::new());

/// Target-side observer for the backend's phase timings, in nanoseconds.
pub fn observe(phase: Phase, duration: Duration) {
    let mut phases = PHASES.lock().expect("phase histograms");
    if let std::collections::btree_map::Entry::Vacant(entry) = phases.entry(phase) {
        let Ok(histogram) = metrics::nanosecond_histogram() else { return };
        entry.insert(histogram);
    }
    let nanos = u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX).max(1);
    let _ = phases.get_mut(&phase).expect("phase histogram").record(nanos);
}

pub fn reset_phases() {
    PHASES.lock().expect("phase histograms").clear();
}

/// Writes `measured-phase-<name>-ns.hdr` files and returns microsecond distributions per phase.
pub fn report_phases(output: &Path) -> Result<Value> {
    let phases = PHASES.lock().expect("phase histograms");
    let mut report = serde_json::Map::new();
    for (phase, histogram) in phases.iter() {
        let name = format!("{phase:?}").to_lowercase();
        V2Serializer::new()
            .serialize(histogram, &mut fs::File::create(output.join(format!("measured-phase-{name}-ns.hdr")))?)?;
        report.insert(name, metrics::nanosecond_distribution(histogram));
    }
    Ok(report.into())
}
