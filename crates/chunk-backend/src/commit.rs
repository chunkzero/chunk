use std::{
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

use chunk_store::{Commit, Operation, Reply, Request, Revision, Snapshot, Storage, Write};
use tokio::sync::mpsc::Sender;

use crate::{
    Error, Result,
    service::{Event, Update},
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
    Activate {
        deployment: Arc<chunk_contract::Deployment>,
    },
    Commit {
        expected: Revision,
        operation: Operation,
        writes: Vec<Write>,
        result: Arc<str>,
        intents: Vec<chunk_store::JobIntent>,
    },
    Scheduling {
        command: chunk_store::JobCommand,
    },
}

/// Prepares and commits that may share one durable write, bounded to keep
/// acknowledgement latency low.
const MAX_BATCH: usize = 64;

pub(crate) struct Committer {
    jobs: Option<mpsc::SyncSender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Committer {
    pub fn new(
        mut store: Box<dyn Storage>,
        events: Sender<Event>,
    ) -> Result<(Self, Snapshot, Vec<chunk_contract::Deployment>, chunk_store::Jobs)> {
        let (jobs, incoming) = mpsc::sync_channel::<Job>(64);
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("chunk-commit".into()).spawn(move || {
            let initial = (|| -> Result<_> {
                Ok((store.snapshot()?, store.deployments()?, store.job_command(chunk_store::JobCommand::Recover)?))
            })();
            if ready.send(initial).is_err() {
                return;
            }
            let mut failed = false;
            let mut next = None;
            while let Some(job) = next.take().or_else(|| incoming.recv().ok()) {
                let batch = if matches!(job, Job::Prepare { .. } | Job::Commit { .. }) {
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
                    durable(store.as_mut(), batch, &mut failed)
                } else {
                    vec![run(store.as_mut(), job, &mut failed)]
                };
                for event in batch {
                    if events.blocking_send(event).is_err() {
                        return;
                    }
                }
            }
        })?;
        let committer = Self { jobs: Some(jobs), thread: Some(thread) };
        let (snapshot, deployments, scheduled) = initialized.recv().map_err(|_| Error::Closed)??;
        Ok((committer, snapshot, deployments, scheduled))
    }

    pub fn send(&self, job: Job) -> Result<()> {
        self.jobs.as_ref().ok_or(Error::Closed)?.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => Error::Busy,
            mpsc::TrySendError::Disconnected(_) => Error::Closed,
        })
    }
}

fn run(store: &mut dyn Storage, job: Job, failed: &mut bool) -> Event {
    match job {
        Job::Release { id } => {
            let result =
                if *failed { Err(Error::CommitFailed) } else { store.release_deployment(&id).map_err(Error::from) };
            *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
            Event::Released { result }
        }
        Job::Activate { deployment } => {
            let result = if *failed {
                Err(Error::CommitFailed)
            } else {
                store.activate_deployment(&deployment).map_err(Error::from).and_then(|revision| {
                    let snapshot = store.snapshot().map_err(|_| Error::CommitFailed)?;
                    if snapshot.revision != revision {
                        return Err(Error::CommitFailed);
                    }
                    Ok(snapshot)
                })
            };
            *failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
            Event::Activated { result }
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
        job @ (Job::Prepare { .. } | Job::Commit { .. }) => {
            durable(store, vec![job], failed).pop().expect("one event per job")
        }
    }
}

enum Queued {
    Prepare { id: String },
    Commit { id: String, expected: Revision, json: Arc<str>, has_jobs: bool },
}

/// Applies prepares and commits in one batch and reports one event per job, in
/// order. Every commit acknowledged by a batch shares the snapshot taken after it.
fn durable(store: &mut dyn Storage, batch: Vec<Job>, failed: &mut bool) -> Vec<Event> {
    let mut requests = Vec::new();
    let mut queued = Vec::with_capacity(batch.len());
    for job in batch {
        let submitted = !*failed && requests.len() == queued.len();
        match job {
            Job::Prepare { operation, context } => {
                queued.push(Queued::Prepare { id: operation.id.clone() });
                if submitted {
                    requests.push(Request::Prepare { operation, context });
                }
            }
            Job::Commit { expected, operation, writes, result, intents } => {
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
    let submitted = requests.len();
    let timer = Timer::start();
    let results = store.batch(requests);
    let committed = |result: &chunk_store::Result<Reply>| match result {
        Ok(Reply::Committed(outcome)) => Some(outcome.revision),
        _ => None,
    };
    let last = results.iter().filter_map(committed).max();
    let snapshot = last.map(|last| match store.snapshot() {
        Ok(snapshot) if snapshot.revision == last => Ok(snapshot),
        _ => Err(Error::CommitFailed),
    });
    let with_jobs = results
        .iter()
        .zip(&queued)
        .any(|(result, queued)| committed(result).is_some() && matches!(queued, Queued::Commit { has_jobs: true, .. }));
    let jobs = with_jobs.then(|| store.jobs().map_err(|_| Error::CommitFailed));
    // Every submitted job waited for the whole durable write.
    for queued in &queued[..submitted] {
        timer.stop(if matches!(queued, Queued::Prepare { .. }) { Phase::Prepare } else { Phase::Commit });
    }
    let mut results = results.into_iter();
    queued
        .into_iter()
        .map(|queued| {
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
                        Some(Ok(Reply::Committed(outcome)))
                            if expected.0.checked_add(1) == Some(outcome.revision.0) =>
                        {
                            (|| {
                                let snapshot = snapshot.clone().ok_or(Error::CommitFailed)??;
                                let jobs =
                                    if has_jobs { Some(jobs.clone().ok_or(Error::CommitFailed)??) } else { None };
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
        })
        .collect()
}

impl Drop for Committer {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
