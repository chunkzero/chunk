//! Sync protocol workloads: `queries` topic streams and app writes over the `chunk.sync.v1.Core` service that core
//! serves beside control, running the compiled backend bundle.
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use chunk_proto::sync::v1::{
    CallRequest, SubscribeRequest, Update, call_response::Outcome, core_client::CoreClient, entry::State,
};
use hdrhistogram::{
    Histogram,
    serialization::{Serializer, V2Serializer},
};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tonic::{Request, transport::Channel};

use crate::{
    backend::SEED_BATCH,
    config::{Config, Writes},
    metrics,
};

const DEPLOYMENT: &str = "bench";
const QUERY: &str = r#"{"top": {"function": "shared/leaderboard/top", "arguments": {}}}"#;

/// Core's sync endpoint and the credentials it accepts from outside a JVM.
#[derive(Clone, Serialize, Deserialize)]
pub struct Connection {
    pub endpoint: String,
    /// Control's credential, which the CLI presents.
    pub cli: String,
    /// The backend's platform credential, which gateways present.
    pub platform: String,
}

async fn connect(endpoint: &str) -> Result<CoreClient<Channel>> {
    Ok(CoreClient::connect(endpoint.to_owned()).await?.max_decoding_message_size(16 << 20))
}

fn request<T>(body: T, credential: &str) -> Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert("authorization", format!("Bearer {credential}").parse()?);
    Ok(request)
}

/// Runs a mutation and returns the revision it committed.
async fn mutate(
    rpc: &mut CoreClient<Channel>,
    credential: &str,
    operation: String,
    method: &str,
    arguments: &Value,
) -> Result<u64> {
    let call = CallRequest {
        operation_id: operation,
        method: method.into(),
        arguments: arguments.to_string().into_bytes(),
        deployment: DEPLOYMENT.into(),
        ..CallRequest::default()
    };
    let response = rpc.call(request(call, credential)?).await?.into_inner();
    match response.outcome {
        Some(Outcome::Result(_)) => {}
        Some(Outcome::Error(error)) => bail!("{:?}: {}", error.code(), error.message),
        None => bail!("call without an outcome"),
    }
    Ok(response.position.context("write without a position")?.revision)
}

/// Seeds player profiles in batches through the bundle's own mutation, as the CLI.
pub async fn seed(connection: &Connection, population: u32) -> Result<()> {
    let mut rpc = connect(&connection.endpoint).await?;
    for first in (0..population).step_by(SEED_BATCH as usize) {
        let arguments = json!({"first": first, "count": SEED_BATCH.min(population - first)});
        mutate(&mut rpc, &connection.cli, format!("seed-{first}"), "shared/players/seed", &arguments)
            .await
            .with_context(|| format!("seeding players from {first}"))?;
    }
    Ok(())
}

pub struct Writer {
    rpc: CoreClient<Channel>,
    credential: String,
    streams: Arc<Streams>,
}

impl Writer {
    /// Writes as the CLI, or with the subscribers' gateway credential under `--own-writes`.
    pub async fn connect(connection: &Connection, config: &Config, streams: Arc<Streams>) -> Result<Self> {
        let credential = if config.own_writes { &connection.platform } else { &connection.cli };
        Ok(Self { rpc: connect(&connection.endpoint).await?, credential: credential.clone(), streams })
    }

    pub async fn execute(&mut self, sequence: u64, config: &Config) -> Result<()> {
        let (operation, method, arguments) = match config.writes {
            Writes::Unrelated => (format!("record-{sequence}"), "shared/activity/record", json!({})),
            Writes::Related => {
                let player = format!("p{}", sequence % u64::from(config.population));
                (format!("raise-{sequence}"), "shared/leaderboard/raise", json!({"player": player}))
            }
        };
        let revision = mutate(&mut self.rpc, &self.credential, operation, method, &arguments).await?;
        self.streams.committed(revision);
        Ok(())
    }
}

struct Positions {
    /// Latest revision each prompt reader observed; slow readers are not tracked.
    seen: Vec<u64>,
    /// When each write's reply arrived, by revision, until every prompt reader observed it.
    replies: BTreeMap<u64, Instant>,
    /// Write reply until a prompt reader observed its position, once per write and reader.
    lag: Histogram<u64>,
}

/// Counts what subscription streams deliver and how long each write's position takes to reach them.
pub struct Streams {
    positions: Mutex<Positions>,
    total: u32,
    connections: u32,
    since: Mutex<Instant>,
    updates: AtomicU64,
    position_only: AtomicU64,
    upserts: AtomicU64,
    entry_errors: AtomicU64,
    bytes: AtomicU64,
    ended: AtomicU64,
    last_end: Mutex<Option<String>>,
}

impl Streams {
    /// Tracks `total` streams, of which the first `prompt` read promptly.
    fn new(total: u32, prompt: u32, connections: u32) -> Result<Self> {
        Ok(Self {
            positions: Mutex::new(Positions {
                seen: vec![0; prompt as usize],
                replies: BTreeMap::new(),
                lag: metrics::histogram()?,
            }),
            total,
            connections,
            since: Mutex::new(Instant::now()),
            updates: AtomicU64::new(0),
            position_only: AtomicU64::new(0),
            upserts: AtomicU64::new(0),
            entry_errors: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            ended: AtomicU64::new(0),
            last_end: Mutex::new(None),
        })
    }

    fn receive(&self, stream: usize, update: &Update) {
        let now = Instant::now();
        if let Some(error) = &update.error {
            return self.end(format!("{:?}: {}", error.code(), error.message));
        }
        self.updates.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(update.encoded_len() as u64, Ordering::Relaxed);
        if !update.snapshot && update.upserts.is_empty() && update.removed.is_empty() {
            self.position_only.fetch_add(1, Ordering::Relaxed);
        }
        let failed = update.upserts.iter().filter(|entry| matches!(entry.state, Some(State::Error(_)))).count();
        self.upserts.fetch_add((update.upserts.len() - failed) as u64, Ordering::Relaxed);
        self.entry_errors.fetch_add(failed as u64, Ordering::Relaxed);
        let Some(revision) = update.position.map(|position| position.revision) else { return };
        let mut positions = self.positions.lock().expect("stream positions");
        let Positions { seen, replies, lag } = &mut *positions;
        let Some(seen) = seen.get_mut(stream).filter(|seen| **seen < revision) else { return };
        for replied in replies.range(*seen + 1..=revision).map(|(_, replied)| *replied) {
            let _ = lag.record(metrics::micros(now.saturating_duration_since(replied)));
        }
        *seen = revision;
    }

    /// Records a write's reply. Readers that observed its position before the reply arrived count as no lag.
    fn committed(&self, revision: u64) {
        let mut positions = self.positions.lock().expect("stream positions");
        let observed = positions.seen.iter().filter(|seen| **seen >= revision).count() as u64;
        if observed > 0 {
            let _ = positions.lag.record_n(1, observed);
        }
        positions.replies.insert(revision, Instant::now());
        let floor = positions.seen.iter().min().copied().unwrap_or(u64::MAX);
        while let Some(entry) = positions.replies.first_entry()
            && *entry.key() <= floor
        {
            entry.remove();
        }
    }

    fn end(&self, reason: String) {
        self.ended.fetch_add(1, Ordering::Relaxed);
        *self.last_end.lock().expect("stream end") = Some(reason);
    }

    pub fn reset(&self) {
        self.positions.lock().expect("stream positions").lag.reset();
        *self.since.lock().expect("measurement start") = Instant::now();
        for counter in [&self.updates, &self.position_only, &self.upserts, &self.entry_errors, &self.bytes] {
            counter.store(0, Ordering::Relaxed);
        }
    }

    pub fn summary(&self, output: &Path) -> Result<Value> {
        let seconds = self.since.lock().expect("measurement start").elapsed().as_secs_f64();
        let rate = |counter: &AtomicU64| metrics::count(counter.load(Ordering::Relaxed)) / seconds;
        let positions = self.positions.lock().expect("stream positions");
        V2Serializer::new().serialize(&positions.lag, &mut fs::File::create(output.join("measured-observed.hdr"))?)?;
        Ok(json!({
            "streams": self.total,
            "slow_streams": self.total as usize - positions.seen.len(),
            "connections": self.connections,
            "window_including_drain_seconds": seconds,
            "updates_per_second": rate(&self.updates),
            "position_only_updates_per_second": rate(&self.position_only),
            "changed_entries_per_second": rate(&self.upserts),
            "update_mib_per_second": rate(&self.bytes) / 1_048_576.0,
            "entry_errors": self.entry_errors.load(Ordering::Relaxed),
            "streams_ended": self.ended.load(Ordering::Relaxed),
            "last_stream_end": *self.last_end.lock().expect("stream end"),
            "reply_to_observed_us": metrics::distribution(&positions.lag),
        }))
    }
}

/// Opens `subscribers` `queries` streams on the leaderboard with the gateway credential, `streams_per_connection` to
/// a connection, and waits for each snapshot. The last `slow_readers` streams wait before each read.
pub async fn subscribe(connection: &Connection, config: &Config) -> Result<Arc<Streams>> {
    let total = config.subscribers();
    let prompt = total - config.slow_readers;
    let streams = Arc::new(Streams::new(total, prompt, total.div_ceil(config.streams_per_connection))?);
    let mut rpc = connect(&connection.endpoint).await?;
    for index in 0..total {
        if index > 0 && index % config.streams_per_connection == 0 {
            rpc = connect(&connection.endpoint).await?;
        }
        let subscription = SubscribeRequest {
            topic: "queries".into(),
            arguments: QUERY.into(),
            deployment: DEPLOYMENT.into(),
            ..SubscribeRequest::default()
        };
        let mut updates = rpc
            .subscribe(request(subscription, &connection.platform)?)
            .await
            .with_context(|| format!("opening stream {} of {total}", index + 1))?
            .into_inner();
        let first = tokio::time::timeout(Duration::from_secs(10), updates.message())
            .await
            .context("waiting for a snapshot")??
            .context("stream closed before its snapshot")?;
        if let Some(error) = first.error {
            bail!("stream {} of {total} rejected: {:?}: {}", index + 1, error.code(), error.message);
        }
        ensure!(first.snapshot && !first.stream.is_empty(), "stream started without a snapshot");
        let index = index as usize;
        streams.receive(index, &first);
        let slow = (index >= prompt as usize).then(|| Duration::from_millis(u64::from(config.slow_read_ms)));
        let streams = streams.clone();
        tokio::spawn(async move {
            loop {
                if let Some(delay) = slow {
                    tokio::time::sleep(delay).await;
                }
                match updates.message().await {
                    Ok(Some(update)) if update.error.is_none() => streams.receive(index, &update),
                    Ok(Some(update)) => return streams.receive(index, &update),
                    Ok(None) => return streams.end("closed without an error".into()),
                    Err(status) => return streams.end(format!("grpc/{:?}: {}", status.code(), status.message())),
                }
            }
        });
    }
    streams.reset();
    Ok(streams)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chunk_proto::sync::v1::Position;

    #[test]
    fn each_write_counts_once_per_prompt_stream_even_if_observed_before_its_reply() {
        let streams = Streams::new(3, 2, 1).unwrap();
        let update = |revision| Update { position: Some(Position { epoch: 1, revision }), ..Update::default() };
        streams.receive(0, &update(1));
        streams.committed(2);
        // Stream 0 observes write 3 before its reply arrives.
        streams.receive(0, &update(3));
        streams.committed(3);
        streams.receive(1, &update(3));
        // The slow stream is not tracked.
        streams.receive(2, &update(3));
        // The next reply drops those every prompt stream observed.
        streams.committed(4);
        let positions = streams.positions.lock().unwrap();
        assert_eq!(positions.lag.len(), 4);
        assert_eq!(positions.replies.keys().copied().collect::<Vec<_>>(), [4]);
    }
}
