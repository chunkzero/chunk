//! Sync protocol workloads: `queries` topic streams and app writes over the `chunk.sync.v1.Core` service that core
//! serves beside control, running the compiled backend bundle.
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail, ensure};
use chunk_proto::sync::v1::{
    CallRequest, Entry, Position, SubscribeRequest, Update, call_response::Outcome, core_client::CoreClient,
    entry::State,
};
use hdrhistogram::Histogram;
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tonic::{Request, transport::Channel};

use crate::{
    backend::SEED_BATCH,
    config::{Config, Writes},
    metrics,
};

pub const DEPLOYMENT: &str = "bench";
/// How long after the last write's reply prompt readers may take to observe it before it counts as unobserved.
const DRAIN: Duration = Duration::from_secs(2);
const QUERY: &str = r#"{"top": {"function": "shared/leaderboard/top", "arguments": {}}}"#;

/// Core's sync endpoint and the credentials it accepts from outside a JVM.
#[derive(Clone, Serialize, Deserialize)]
pub struct Connection {
    pub endpoint: String,
    /// Control's credential, which the CLI presents.
    pub cli: String,
    /// The in-process gateway's ID and credential.
    pub gateway_id: String,
    pub gateway: String,
}

pub async fn connect(endpoint: &str) -> Result<CoreClient<Channel>> {
    Ok(CoreClient::connect(endpoint.to_owned()).await?.max_decoding_message_size(16 << 20))
}

pub fn request<T>(body: T, credential: &str) -> Result<Request<T>> {
    let mut request = Request::new(body);
    request.metadata_mut().insert("authorization", format!("Bearer {credential}").parse()?);
    Ok(request)
}

/// Makes `message` as `credential`, returning its result and position, or failing with the error core returned.
pub async fn call(
    rpc: &mut CoreClient<Channel>,
    credential: &str,
    message: CallRequest,
) -> Result<(Vec<u8>, Option<Position>)> {
    let response = rpc.call(request(message, credential)?).await?.into_inner();
    match response.outcome {
        Some(Outcome::Result(result)) => Ok((result, response.position)),
        Some(Outcome::Error(error)) => bail!("{:?}: {}", error.code(), error.message),
        None => bail!("call without an outcome"),
    }
}

/// Runs a mutation and returns the revision it committed.
async fn mutate(
    rpc: &mut CoreClient<Channel>,
    credential: &str,
    operation: String,
    method: &str,
    arguments: &Value,
) -> Result<u64> {
    let message = CallRequest {
        operation_id: operation,
        method: method.into(),
        arguments: arguments.to_string().into_bytes(),
        deployment: DEPLOYMENT.into(),
        ..CallRequest::default()
    };
    let (_, position) = call(rpc, credential, message).await?;
    Ok(position.context("write without a position")?.revision)
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
        let credential = if config.own_writes { &connection.gateway } else { &connection.cli };
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
        self.streams.committed(revision, Instant::now());
        Ok(())
    }
}

/// A prompt reader's last update.
#[derive(Clone, Copy)]
struct Seen {
    revision: u64,
    /// The revision it had observed before that update.
    before: u64,
    at: Instant,
}

struct Positions {
    /// Slow readers are not tracked.
    seen: Vec<Seen>,
    /// When each measured write's reply arrived, by revision, until every prompt reader observed it.
    replies: BTreeMap<u64, Instant>,
    /// Write reply until a prompt reader observed its position, once per write and reader.
    lag: Histogram<u64>,
}

impl Positions {
    /// Write and prompt reader pairs whose position the reader has yet to observe.
    fn unobserved(&self) -> u64 {
        let mut seen: Vec<_> = self.seen.iter().map(|seen| seen.revision).collect();
        seen.sort_unstable();
        self.replies.keys().map(|revision| seen.partition_point(|seen| seen < revision) as u64).sum()
    }
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
        let now = Instant::now();
        Ok(Self {
            positions: Mutex::new(Positions {
                seen: vec![Seen { revision: 0, before: 0, at: now }; prompt as usize],
                replies: BTreeMap::new(),
                lag: metrics::histogram()?,
            }),
            total,
            connections,
            since: Mutex::new(now),
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
        let Some(seen) = seen.get_mut(stream).filter(|seen| seen.revision < revision) else { return };
        for replied in replies.range(seen.revision + 1..=revision).map(|(_, replied)| *replied) {
            let _ = lag.record(metrics::micros(now.saturating_duration_since(replied)));
        }
        *seen = Seen { revision, before: seen.revision, at: now };
    }

    /// Records a write's reply, which arrived at `replied`. A reader whose update reached the write's position
    /// before this runs counts from that update; one that had reached it earlier counts as no lag.
    fn committed(&self, revision: u64, replied: Instant) {
        let mut positions = self.positions.lock().expect("stream positions");
        let Positions { seen, replies, lag } = &mut *positions;
        for seen in seen.iter().filter(|seen| seen.revision >= revision) {
            let observed = if seen.before < revision { seen.at } else { replied };
            let _ = lag.record(metrics::micros(observed.saturating_duration_since(replied)));
        }
        replies.insert(revision, replied);
        let floor = seen.iter().map(|seen| seen.revision).min().unwrap_or(u64::MAX);
        while let Some(entry) = replies.first_entry()
            && *entry.key() <= floor
        {
            entry.remove();
        }
    }

    fn end(&self, reason: String) {
        self.ended.fetch_add(1, Ordering::Relaxed);
        *self.last_end.lock().expect("stream end") = Some(reason);
    }

    /// Starts the measurement, dropping earlier writes. Fails if a query failed or a stream ended before it.
    pub fn reset(&self) -> Result<()> {
        let (errors, ended) = (self.entry_errors.load(Ordering::Relaxed), self.ended.load(Ordering::Relaxed));
        ensure!(
            errors == 0 && ended == 0,
            "{errors} query errors and {ended} ended streams before the measurement; last end: {:?}",
            self.last_end.lock().expect("stream end")
        );
        let mut positions = self.positions.lock().expect("stream positions");
        positions.lag.reset();
        positions.replies.clear();
        *self.since.lock().expect("measurement start") = Instant::now();
        for counter in [&self.updates, &self.position_only, &self.upserts, &self.bytes] {
            counter.store(0, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Waits up to [`DRAIN`] for prompt readers to observe every measured write.
    pub async fn drain(&self) {
        let deadline = Instant::now() + DRAIN;
        while self.positions.lock().expect("stream positions").unobserved() > 0 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The measurement at this instant and its lag histogram.
    pub fn freeze(&self) -> (Value, Histogram<u64>) {
        let positions = self.positions.lock().expect("stream positions");
        let seconds = self.since.lock().expect("measurement start").elapsed().as_secs_f64();
        let rate = |counter: &AtomicU64| metrics::count(counter.load(Ordering::Relaxed)) / seconds;
        let summary = json!({
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
            "unobserved_pairs": positions.unobserved(),
        });
        (summary, positions.lag.clone())
    }
}

/// Opens `subscribers` `queries` streams on the leaderboard with the gateway credential, `streams_per_connection` to
/// a connection, and waits for each snapshot. The last `slow_readers` streams wait before each read, on connections
/// no prompt stream shares.
pub async fn subscribe(connection: &Connection, config: &Config) -> Result<Arc<Streams>> {
    let total = config.subscribers;
    let prompt = total - config.slow_readers;
    let per_connection = config.streams_per_connection;
    let connections = prompt.div_ceil(per_connection) + config.slow_readers.div_ceil(per_connection);
    let streams = Arc::new(Streams::new(total, prompt, connections)?);
    let mut rpc = connect(&connection.endpoint).await?;
    for index in 0..total {
        let offset = if index < prompt { index } else { index - prompt };
        if index > 0 && offset % per_connection == 0 {
            rpc = connect(&connection.endpoint).await?;
        }
        let subscription = SubscribeRequest {
            topic: "queries".into(),
            arguments: QUERY.into(),
            deployment: DEPLOYMENT.into(),
            ..SubscribeRequest::default()
        };
        let mut updates = rpc
            .subscribe(request(subscription, &connection.gateway)?)
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
        match first.upserts.as_slice() {
            [Entry { state: Some(State::Value(_)), .. }] => {}
            [Entry { state: Some(State::Error(error)), .. }] => {
                bail!("stream {} of {total} query failed: {:?}: {}", index + 1, error.code(), error.message);
            }
            entries => bail!("stream {} of {total} started with {} entries, not one result", index + 1, entries.len()),
        }
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
    streams.reset()?;
    Ok(streams)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(revision: u64) -> Update {
        Update { position: Some(Position { epoch: 1, revision }), ..Update::default() }
    }

    #[test]
    fn each_write_counts_once_per_prompt_stream_even_if_observed_before_its_reply() {
        let streams = Streams::new(3, 2, 1).unwrap();
        streams.receive(0, &update(1));
        streams.committed(2, Instant::now());
        // Stream 0 observes write 3 before its reply is recorded.
        let replied = Instant::now();
        streams.receive(0, &update(3));
        streams.committed(3, replied);
        streams.receive(1, &update(3));
        // The slow stream is not tracked.
        streams.receive(2, &update(3));
        // The next reply drops those every prompt stream observed.
        streams.committed(4, Instant::now());
        let positions = streams.positions.lock().unwrap();
        assert_eq!(positions.lag.len(), 4);
        assert_eq!(positions.replies.keys().copied().collect::<Vec<_>>(), [4]);
    }

    #[test]
    fn warmup_writes_are_not_measured_and_unobserved_pairs_are_counted() {
        let streams = Streams::new(2, 2, 1).unwrap();
        streams.committed(1, Instant::now());
        streams.reset().unwrap();
        streams.committed(2, Instant::now());
        streams.receive(0, &update(2));
        let (summary, lag) = streams.freeze();
        assert_eq!(lag.len(), 1);
        assert_eq!(summary["unobserved_pairs"], 1);
    }
}
