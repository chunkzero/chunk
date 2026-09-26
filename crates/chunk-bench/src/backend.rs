//! Backend workloads: the TypeScript bundle in `backend/`, compiled like an app and served by the production backend
//! over authenticated gRPC with on-disk SQLite.
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use chunk_backend::Phase;
use chunk_contract::BackendConnection;
use chunk_proto::v1::{
    BackendMutation, BackendQuery, BackendResult, BackendUpdate, BackendWatchGroup, backend_client::BackendClient,
};
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tonic::{Request, metadata::MetadataMap, transport::Channel};

use crate::{
    config::{Config, Scenario, Subscription},
    metrics,
};

pub const ENVIRONMENT: &str = "bench";
const SEED_BATCH: u32 = 200;
const SOURCES: [(&str, &str); 3] = [
    ("server/schema/index.ts", include_str!("../backend/server/schema/index.ts")),
    ("server/players.ts", include_str!("../backend/server/players.ts")),
    ("server/leaderboard.ts", include_str!("../backend/server/leaderboard.ts")),
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

fn caller(player: u64) -> Vec<u8> {
    json!({"session": "bench-session", "app": "bench", "player": format!("p{player}")}).to_string().into_bytes()
}

fn save(sequence: u64) -> Value {
    let inventory: Vec<_> =
        (0..16).map(|slot| json!({"item": format!("item-{}", (sequence + slot) % 64), "count": 1 + slot})).collect();
    json!({"coins": sequence % 10_000, "xp": sequence * 3, "level": 1 + sequence % 50,
        "best": sequence * 7 % 100_000, "inventory": inventory})
}

pub struct Client {
    rpc: BackendClient<Channel>,
    metadata: MetadataMap,
    fanout: Option<Arc<Fanout>>,
}

impl Client {
    pub async fn connect(connection: &BackendConnection, fanout: Option<Arc<Fanout>>) -> Result<Self> {
        let mut metadata = MetadataMap::new();
        metadata.insert("authorization", format!("Bearer {}", connection.token).parse()?);
        metadata.insert("x-chunk-environment", connection.environment.parse()?);
        metadata.insert("x-chunk-deployment", connection.deployment.parse()?);
        let rpc = BackendClient::connect(connection.endpoint.clone()).await?.max_decoding_message_size(2 << 20);
        Ok(Self { rpc, metadata, fanout })
    }

    fn request<T>(&self, body: T) -> Request<T> {
        let mut request = Request::new(body);
        *request.metadata_mut() = self.metadata.clone();
        request
    }

    async fn mutate(
        &mut self,
        operation: String,
        function: &str,
        arguments: &Value,
        player: u64,
    ) -> Result<BackendResult> {
        let mutation = BackendMutation {
            function: function.into(),
            arguments_json: arguments.to_string().into_bytes(),
            caller_json: caller(player),
            operation_id: operation,
        };
        Ok(self.rpc.mutate(self.request(mutation)).await?.into_inner())
    }

    pub async fn execute(&mut self, sequence: u64, config: &Config) -> Result<()> {
        let player = sequence % u64::from(config.population);
        match config.scenario {
            Scenario::BackendQuery => {
                let query = BackendQuery {
                    function: "shared/players/load".into(),
                    arguments_json: b"{}".to_vec(),
                    caller_json: caller(player),
                };
                let result = self.rpc.query(self.request(query)).await?.into_inner();
                ensure!(result.result_json.starts_with(b"{"), "profile missing");
            }
            Scenario::BackendMutation => {
                let result =
                    self.mutate(format!("save-{sequence}"), "shared/players/save", &save(sequence), player).await?;
                ensure!(std::str::from_utf8(&result.result_json)?.parse::<u64>().is_ok(), "unexpected save result");
            }
            Scenario::BackendFanout => {
                let result =
                    self.mutate(format!("submit-{sequence}"), "shared/leaderboard/submit", &json!({}), player).await?;
                let replied = Instant::now();
                let fanout = self.fanout.as_ref().context("fan-out subscriptions missing")?;
                fanout.delivered(result.revision).await?;
                fanout.after_reply.lock().expect("fan-out histogram").record(metrics::micros(replied.elapsed()))?;
            }
            _ => anyhow::bail!("not a backend workload"),
        }
        Ok(())
    }
}

/// Seeds player profiles in batches through the bundle's own mutation.
pub async fn seed(connection: &BackendConnection, population: u32) -> Result<()> {
    let mut client = Client::connect(connection, None).await?;
    for first in (0..population).step_by(SEED_BATCH as usize) {
        let count = SEED_BATCH.min(population - first);
        let arguments = json!({"first": first, "count": count});
        let result = client
            .mutate(format!("seed-{first}"), "shared/players/seed", &arguments, 0)
            .await
            .with_context(|| format!("seeding players {first}..{}", first + count))?;
        ensure!(result.result_json == count.to_string().as_bytes(), "seed count mismatch");
    }
    Ok(())
}

#[derive(Default)]
struct Deliveries {
    /// Latest revision received on each stream.
    seen: Vec<u64>,
    /// Awaited revision -> (streams at or past it, waiters).
    waiting: BTreeMap<u64, (usize, Vec<oneshot::Sender<()>>)>,
}

/// Tracks subscription streams so a write completes only once every stream has delivered its revision.
pub struct Fanout {
    deliveries: Mutex<Deliveries>,
    after_reply: Mutex<Histogram<u64>>,
    updates: AtomicU64,
    errors: AtomicU64,
}

impl Fanout {
    fn receive(&self, stream: usize, update: &BackendUpdate) {
        self.updates.fetch_add(1, Ordering::Relaxed);
        if update.errors.iter().any(|error| !error.is_empty()) {
            self.errors.fetch_add(1, Ordering::Relaxed);
        }
        let mut deliveries = self.deliveries.lock().expect("fan-out state");
        let previous = std::mem::replace(&mut deliveries.seen[stream], update.revision);
        if update.revision <= previous {
            return;
        }
        let streams = deliveries.seen.len();
        let complete: Vec<_> = deliveries
            .waiting
            .range_mut(previous + 1..=update.revision)
            .filter_map(|(revision, (count, _))| {
                *count += 1;
                (*count == streams).then_some(*revision)
            })
            .collect();
        for revision in complete {
            for waiter in deliveries.waiting.remove(&revision).map(|(_, waiters)| waiters).unwrap_or_default() {
                let _ = waiter.send(());
            }
        }
    }

    async fn delivered(&self, revision: u64) -> Result<()> {
        let receiver = {
            let mut deliveries = self.deliveries.lock().expect("fan-out state");
            let reached = deliveries.seen.iter().filter(|seen| **seen >= revision).count();
            if reached == deliveries.seen.len() {
                return Ok(());
            }
            let (sender, receiver) = oneshot::channel();
            deliveries.waiting.entry(revision).or_insert((reached, Vec::new())).1.push(sender);
            receiver
        };
        receiver.await.context("subscription stream closed")
    }

    pub fn reset(&self) {
        self.after_reply.lock().expect("fan-out histogram").reset();
        self.updates.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
    }

    pub fn summary(&self, output: &Path) -> Result<Value> {
        let histogram = self.after_reply.lock().expect("fan-out histogram");
        V2Serializer::new().serialize(&*histogram, &mut fs::File::create(output.join("measured-delivery.hdr"))?)?;
        Ok(json!({
            "streams": self.deliveries.lock().expect("fan-out state").seen.len(),
            "updates_received": self.updates.load(Ordering::Relaxed),
            "updates_with_query_errors": self.errors.load(Ordering::Relaxed),
            "reply_to_all_delivered_us": metrics::distribution(&histogram),
        }))
    }
}

/// Opens `subscribers` query subscriptions in watch groups of `group_size`, one connection per group, and waits for
/// every initial result. The backend's own admission limits decide how many are accepted.
pub async fn subscribe(connection: &BackendConnection, config: &Config) -> Result<Arc<Fanout>> {
    let streams = config.subscribers.div_ceil(config.group_size);
    let fanout = Arc::new(Fanout {
        deliveries: Mutex::new(Deliveries { seen: vec![0; streams as usize], waiting: BTreeMap::new() }),
        after_reply: Mutex::new(metrics::histogram()?),
        updates: AtomicU64::new(0),
        errors: AtomicU64::new(0),
    });
    let function = match config.subscription {
        Subscription::Shared => "shared/leaderboard/top",
        Subscription::PerPlayer => "shared/leaderboard/standing",
    };
    for stream in 0..streams {
        let client = Client::connect(connection, None).await?;
        let first = stream * config.group_size;
        let queries = (first..config.subscribers.min(first + config.group_size))
            .map(|subscriber| BackendQuery {
                function: function.into(),
                arguments_json: b"{}".to_vec(),
                caller_json: caller(u64::from(subscriber % config.population)),
            })
            .collect();
        let mut updates = client
            .rpc
            .clone()
            .watch_group(client.request(BackendWatchGroup { queries }))
            .await
            .with_context(|| format!("opening subscription stream {} of {streams}", stream + 1))?
            .into_inner();
        let initial = updates.message().await?.context("subscription closed before its initial result")?;
        ensure!(
            initial.errors.iter().all(String::is_empty),
            "initial subscription result failed: {:?}",
            initial.errors
        );
        fanout.receive(stream as usize, &initial);
        let fanout = fanout.clone();
        tokio::spawn(async move {
            while let Ok(Some(update)) = updates.message().await {
                fanout.receive(stream as usize, &update);
            }
        });
    }
    fanout.reset();
    Ok(fanout)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_complete_only_after_every_stream_reaches_their_revision() {
        let fanout = Fanout {
            deliveries: Mutex::new(Deliveries { seen: vec![1, 1], waiting: BTreeMap::new() }),
            after_reply: Mutex::new(metrics::histogram().unwrap()),
            updates: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        };
        let update = |revision| BackendUpdate { revision, ..Default::default() };
        fanout.receive(0, &update(3));
        let waiting = fanout.delivered(3);
        tokio::pin!(waiting);
        assert!(tokio::time::timeout(Duration::from_millis(10), &mut waiting).await.is_err());
        // A coalesced update past the awaited revision also completes it.
        fanout.receive(1, &update(5));
        waiting.await.unwrap();
        fanout.delivered(2).await.unwrap();
    }

    /// Needs native TypeScript, like other compiled-bundle tests.
    #[tokio::test(flavor = "multi_thread")]
    async fn fanout_writes_arriving_out_of_order_still_change_every_subscription() {
        use clap::Parser;
        let root = tempfile::tempdir().unwrap();
        let output = root.path().to_owned();
        let bundle = tokio::task::spawn_blocking(move || compile(&output)).await.unwrap().unwrap();
        let (ready, receiver) = oneshot::channel();
        let stop = tokio_util::sync::CancellationToken::new();
        let server = tokio::spawn(chunk_backend::server::run(
            chunk_backend::server::Config {
                bundle: Some(bundle),
                environment: ENVIRONMENT.into(),
                state: root.path().join("state"),
                connection: root.path().join("connection.json"),
                bind: "127.0.0.1:0".parse().unwrap(),
            },
            ready,
            stop.clone(),
        ));
        let connection = receiver.await.unwrap().connection;
        let config = Config::parse_from(["bench", "backend-fanout", "--population", "1", "--subscribers", "2"]);
        seed(&connection, config.population).await.unwrap();
        let fanout = subscribe(&connection, &config).await.unwrap();
        let mut client = Client::connect(&connection, Some(fanout)).await.unwrap();
        // A later offer can reach the backend first; the earlier one must still change the leaderboard.
        for sequence in [1, 0] {
            tokio::time::timeout(Duration::from_secs(5), client.execute(sequence, &config)).await.unwrap().unwrap();
        }
        drop(client);
        stop.cancel();
        server.await.unwrap().unwrap();
    }
}
