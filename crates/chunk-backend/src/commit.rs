use std::{
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

use chunk_store::{
    Commit, DatabaseSchema, Epoch, Operation, PendingWork, Reply, Request, Revision, Snapshot, Storage, Write,
};
use tokio::sync::mpsc::Sender;

use crate::{
    Error, Result,
    service::{Event, Update},
    system::{Lane, OPERATION_PREFIX, SystemJob},
    timing::{Phase, Timer},
};

pub(crate) enum Job {
    Prepare {
        operation: Operation,
        context: chunk_store::RetryContext,
    },
    Release {
        id: String,
    },
    Install {
        deployment: Arc<chunk_contract::Deployment>,
    },
    /// Runs one item of pending work. A backfill runs its transform in `deployment`'s bundle.
    Work {
        id: u64,
        deployment: Option<Arc<chunk_contract::Deployment>>,
    },
    Commit {
        expected: Revision,
        /// System commits the sender had seen when it chose `expected`.
        system: u64,
        operation: Operation,
        writes: Vec<Write>,
        result: Arc<str>,
        intents: Vec<chunk_store::JobIntent>,
    },
    Scheduling {
        command: chunk_store::JobCommand,
    },
    /// Drains the system lane.
    Wake,
}

/// Prepares and commits that may share one durable write, bounded to keep
/// acknowledgement latency low.
const MAX_BATCH: usize = 64;

/// Every system commit records an operation with a unique ID, so they share one request fingerprint.
const FINGERPRINT: [u8; 32] = *b"chunk-environment-system-commit!";

/// What the commit thread found on opening the store.
pub(crate) struct Initial {
    pub snapshot: Snapshot,
    pub deployments: Vec<chunk_contract::Deployment>,
    pub jobs: chunk_store::Jobs,
    pub stored: Stored,
    /// The deployments whose retirement an earlier run committed.
    pub retiring: Vec<String>,
}

/// The store's pending work and its active expand migrations.
pub(crate) struct Stored {
    pub work: Vec<PendingWork>,
    pub migrations: Vec<chunk_contract::Migration>,
}

/// How far the log has advanced.
struct Sequence {
    revision: Revision,
    /// System commits so far. An app commit's expected revision moves past those its sender had not seen.
    system: u64,
    epoch: Epoch,
}

pub(crate) struct Committer {
    jobs: Option<mpsc::SyncSender<Job>>,
    lane: Arc<Lane>,
    thread: Option<JoinHandle<()>>,
}

impl Committer {
    pub fn new(mut store: Box<dyn Storage>, events: Sender<Event>) -> Result<(Self, Initial)> {
        let (jobs, incoming) = mpsc::sync_channel::<Job>(64);
        let (ready, initialized) = mpsc::sync_channel(1);
        let lane = Arc::new(Lane::default());
        let (wake, thread_lane) = (jobs.clone(), lane.clone());
        let thread = std::thread::Builder::new().name("chunk-commit".into()).spawn(move || {
            let lane = Closing(thread_lane);
            let initial = (|| -> Result<_> {
                Ok(Initial {
                    snapshot: store.snapshot()?,
                    deployments: store.deployments()?,
                    jobs: store.job_command(chunk_store::JobCommand::Recover)?,
                    stored: stored(store.as_ref())?,
                    retiring: store.retiring()?,
                })
            })();
            let Ok(Initial { snapshot, .. }) = &initial else {
                let _ = ready.send(initial);
                return;
            };
            let revision = snapshot.revision;
            let mut sequence = Sequence { revision, system: 0, epoch: store.epoch() };
            lane.0.start(wake, sequence.epoch);
            if ready.send(initial).is_err() {
                return;
            }
            let mut failed = false;
            let mut next = None;
            let mut migrator = crate::migrations::Migrator::default();
            while let Some(job) = next.take().or_else(|| incoming.recv().ok()) {
                let system = lane.0.take();
                let healthy = !failed;
                let mut batch = match job {
                    job @ (Job::Prepare { .. } | Job::Commit { .. }) => {
                        // Prepares and commits already queued share one durable write.
                        let mut batch = vec![job];
                        while batch.len() < MAX_BATCH {
                            match incoming.try_recv() {
                                Ok(job @ (Job::Prepare { .. } | Job::Commit { .. })) => batch.push(job),
                                Ok(other) => {
                                    next = Some(other);
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        durable(store.as_mut(), &mut sequence, system, batch, &mut failed)
                    }
                    Job::Wake => durable(store.as_mut(), &mut sequence, system, Vec::new(), &mut failed),
                    job => {
                        let mut events = durable(store.as_mut(), &mut sequence, system, Vec::new(), &mut failed);
                        events.push(run(store.as_mut(), &mut sequence, job, &mut migrator, &mut failed));
                        events
                    }
                };
                // The first fatal failure stops the system lane and the engine together.
                if failed && healthy {
                    lane.0.fail();
                    batch.push(Event::Failed);
                }
                for event in batch {
                    if events.blocking_send(event).is_err() {
                        return;
                    }
                }
            }
        })?;
        let committer = Self { jobs: Some(jobs), lane, thread: Some(thread) };
        let initial = initialized.recv().map_err(|_| Error::Closed)??;
        Ok((committer, initial))
    }

    pub fn lane(&self) -> Arc<Lane> {
        self.lane.clone()
    }

    pub fn send(&self, job: Job) -> Result<()> {
        self.jobs.as_ref().ok_or(Error::Closed)?.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Error::Busy,
            mpsc::TrySendError::Disconnected(_) => Error::Closed,
        })
    }
}

/// Closes the lane however the commit thread exits, so no system caller waits forever.
struct Closing(Arc<Lane>);

impl Drop for Closing {
    fn drop(&mut self) {
        self.0.close();
    }
}

fn run(
    store: &mut dyn Storage,
    sequence: &mut Sequence,
    job: Job,
    migrator: &mut crate::migrations::Migrator,
    failed: &mut bool,
) -> Event {
    match job {
        Job::Release { id } => {
            let result = if *failed {
                Err(Error::CommitFailed)
            } else {
                store
                    .release_deployment(&id)
                    .map_err(Error::from)
                    .and_then(|released| Ok((released, stored(store).map_err(|_| Error::CommitFailed)?)))
            };
            *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
            Event::Released { result }
        }
        Job::Install { deployment } => {
            let result = if *failed {
                Err(Error::CommitFailed)
            } else {
                store.install_deployment(&deployment).map_err(Error::from).and_then(|revision| {
                    sequence.revision = revision;
                    let (snapshot, work) = current(store)?;
                    if snapshot.revision != revision {
                        return Err(Error::CommitFailed);
                    }
                    Ok((snapshot, work))
                })
            };
            *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
            Event::Installed { result }
        }
        Job::Work { id, deployment } => {
            let result = if *failed {
                Err(Error::CommitFailed)
            } else {
                let mut transform = |migration: &str, table: &str, row| {
                    let deployment =
                        deployment.as_ref().ok_or("no resident deployment carries the migration".to_owned())?;
                    migrator.to(deployment, migration, table, &row)
                };
                store.run_work(id, &mut transform).map_err(Error::from).and_then(|()| current(store))
            };
            *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
            Event::Worked { id, result }
        }
        Job::Scheduling { command } => {
            let result = if *failed {
                Err(Error::CommitFailed)
            } else {
                store.job_command(command.clone()).map_err(Error::from)
            };
            *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
            Event::Scheduled { command, result }
        }
        Job::Prepare { .. } | Job::Commit { .. } | Job::Wake => unreachable!("durable writes run in batches"),
    }
}

/// A snapshot after a durable write, with the work still pending. Failing to read them is fatal.
fn current(store: &mut dyn Storage) -> Result<(Snapshot, Stored)> {
    let snapshot = store.snapshot().map_err(|_| Error::CommitFailed)?;
    Ok((snapshot, stored(store).map_err(|_| Error::CommitFailed)?))
}

fn stored(store: &dyn Storage) -> chunk_store::Result<Stored> {
    Ok(Stored { work: store.pending_work()?, migrations: store.migrations()? })
}

/// Installs system tables, reporting whether that advanced the revision, and reads the store.
fn open(
    store: &mut dyn Storage,
    sequence: &mut Sequence,
    schema: &DatabaseSchema,
    failed: &mut bool,
) -> (bool, Result<Snapshot>) {
    if *failed {
        return (false, Err(Error::CommitFailed));
    }
    let revision = match store.apply_schema(schema) {
        Ok(revision) => revision,
        Err(error) => {
            let error = Error::from(error);
            *failed |= !error.is_rejected_commit();
            return (false, Err(error));
        }
    };
    let advanced = revision != sequence.revision;
    if advanced {
        sequence.revision = revision;
        sequence.system += 1;
    }
    let snapshot = store.snapshot().map_err(Error::from);
    *failed |= advanced && snapshot.is_err();
    (advanced, snapshot)
}

/// Each submitted system commit's revision and reply.
type Replies = Vec<(Revision, mpsc::SyncSender<Result<Revision>>)>;

/// Installs system tables at once, reporting any revision that advanced, and turns system
/// commits into the leading requests of the next durable write, computing each one's writes
/// from the revision it commits at. A commit whose writes fail is rejected alone.
fn system_requests(
    store: &mut dyn Storage,
    sequence: &mut Sequence,
    system: Vec<SystemJob>,
    failed: &mut bool,
) -> (Vec<Event>, Vec<Request>, Replies) {
    let mut events = Vec::new();
    let (opens, commits): (Vec<_>, Vec<_>) = system.into_iter().partition(|job| matches!(job, SystemJob::Open { .. }));
    for job in opens {
        if let SystemJob::Open { schema, reply } = job {
            let (advanced, result) = open(store, sequence, &schema, failed);
            if advanced {
                let (revision, snapshot) = (sequence.revision, result.clone());
                events.push(Event::System { count: 1, revision, snapshot });
            }
            let _ = reply.send(result);
        }
    }
    let mut requests = Vec::new();
    let mut replies = Vec::new();
    let mut expected = sequence.revision;
    for job in commits {
        if let SystemJob::Commit { writes, reply } = job {
            let Some(revision) = expected.0.checked_add(1).filter(|_| !*failed) else {
                let _ = reply.send(Err(Error::CommitFailed));
                continue;
            };
            let writes = match writes(Revision(revision)) {
                Ok(writes) => writes,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    continue;
                }
            };
            let id = format!("{OPERATION_PREFIX}{}/{revision}", sequence.epoch.0);
            let operation = Operation { id, fingerprint: FINGERPRINT };
            let commit = Commit { expected, operation, writes, result: serde_json::Value::Null };
            requests.push(Request::Commit { commit, intents: Vec::new() });
            replies.push((Revision(revision), reply));
            expected = Revision(revision);
        }
    }
    (events, requests, replies)
}

/// Replies to system commits in order, returning how many committed and the last one's revision.
fn acknowledge(
    replies: Replies,
    results: &mut impl Iterator<Item = chunk_store::Result<Reply>>,
    failed: &mut bool,
) -> (u64, Revision) {
    let mut committed = (0, Revision(0));
    for (revision, reply) in replies {
        let result = match results.next().filter(|_| !*failed) {
            Some(Ok(Reply::Committed(outcome))) if outcome.revision == revision => Ok(revision),
            Some(Err(error)) => Err(Error::from(error)),
            _ => Err(Error::CommitFailed),
        };
        if let Ok(revision) = result {
            committed = (committed.0 + 1, revision);
        }
        *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
        let _ = reply.send(result);
    }
    committed
}

enum Queued {
    Prepare { id: String },
    Commit { id: String, expected: Revision, json: Arc<str>, has_jobs: bool },
}

/// Installs system tables, then applies prepares and commits in one batch. System commits come first,
/// and each app commit's expected revision moves past the system commits its sender had not
/// seen. Reports one [`Event::System`] for system commits, then one event per app job, in
/// order. Every commit acknowledged by a batch shares the snapshot taken after it.
fn durable(
    store: &mut dyn Storage,
    sequence: &mut Sequence,
    system: Vec<SystemJob>,
    batch: Vec<Job>,
    failed: &mut bool,
) -> Vec<Event> {
    let (mut events, mut requests, replies) = system_requests(store, sequence, system, failed);
    // App commits follow every system commit in this batch.
    let system_total = sequence.system + requests.len() as u64;
    let mut queued = Vec::with_capacity(batch.len());
    for job in batch {
        let submitted = !*failed && requests.len() == replies.len() + queued.len();
        match job {
            Job::Prepare { operation, context } => {
                queued.push(Queued::Prepare { id: operation.id.clone() });
                if submitted {
                    requests.push(Request::Prepare { operation, context });
                }
            }
            Job::Commit { expected, system, operation, writes, result, intents } => {
                let expected = Revision(expected.0 + system_total.saturating_sub(system));
                let has_jobs = !intents.is_empty();
                queued.push(Queued::Commit { id: operation.id.clone(), expected, json: result.clone(), has_jobs });
                // Storage currently takes Value; only the durable boundary decodes results.
                if submitted && let Ok(value) = serde_json::from_str(&result) {
                    requests.push(Request::Commit {
                        commit: Commit { expected, operation, writes, result: value },
                        intents,
                    });
                }
            }
            _ => unreachable!("only prepares and commits share a batch"),
        }
    }
    if requests.is_empty() && queued.is_empty() {
        return events;
    }
    let submitted = requests.len() - replies.len();
    let timer = Timer::start();
    let results = store.batch(requests);
    let committed = |result: &chunk_store::Result<Reply>| match result {
        Ok(Reply::Committed(outcome)) => Some(outcome.revision),
        _ => None,
    };
    let last = results.iter().filter_map(committed).max();
    if let Some(last) = last {
        sequence.revision = last;
    }
    let snapshot = last.map(|last| match store.snapshot() {
        Ok(snapshot) if snapshot.revision == last => Ok(snapshot),
        _ => Err(Error::CommitFailed),
    });
    let with_jobs =
        results.iter().skip(replies.len()).zip(&queued).any(|(result, queued)| {
            committed(result).is_some() && matches!(queued, Queued::Commit { has_jobs: true, .. })
        });
    let jobs = with_jobs.then(|| store.jobs().map_err(|_| Error::CommitFailed));
    // Every submitted job waited for the whole durable write.
    for queued in &queued[..submitted] {
        timer.stop(if matches!(queued, Queued::Prepare { .. }) { Phase::Prepare } else { Phase::Commit });
    }
    let mut results = results.into_iter();
    if let (count @ 1.., revision) = acknowledge(replies, &mut results, failed) {
        sequence.system += count;
        let snapshot = snapshot.clone().unwrap_or(Err(Error::CommitFailed));
        *failed |= snapshot.is_err();
        events.push(Event::System { count, revision, snapshot });
    }
    events.extend(queued.into_iter().map(|queued| {
        let reply = results.next().filter(|_| !*failed);
        match queued {
            Queued::Prepare { id } => {
                let result = match reply {
                    Some(Ok(Reply::Prepared(context))) => Ok(context),
                    Some(Err(error)) => Err(Error::from(error)),
                    _ => Err(Error::CommitFailed),
                };
                *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                Event::Prepared { operation: id, result }
            }
            Queued::Commit { id, expected, json, has_jobs } => {
                let result = match reply {
                    Some(Ok(Reply::Committed(outcome))) if expected.0.checked_add(1) == Some(outcome.revision.0) => {
                        (|| {
                            let snapshot = snapshot.clone().ok_or(Error::CommitFailed)??;
                            let jobs = if has_jobs { Some(jobs.clone().ok_or(Error::CommitFailed)??) } else { None };
                            Ok((Update { revision: outcome.revision, json }, snapshot, jobs))
                        })()
                    }
                    Some(Err(error)) => Err(Error::from(error)),
                    _ => Err(Error::CommitFailed),
                };
                *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                Event::Committed { operation: id, result }
            }
        }
    }));
    events
}

impl Drop for Committer {
    fn drop(&mut self) {
        // The lane holds a sender too; the thread stops only once both are gone.
        self.lane.close();
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
