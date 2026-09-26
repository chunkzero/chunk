use std::{
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

use chunk_store::{Commit, Operation, Revision, Snapshot, Storage, Write};
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
            while let Ok(job) = incoming.recv() {
                let event = match job {
                    Job::Prepare { operation, context } => {
                        let result = if failed {
                            Err(Error::CommitFailed)
                        } else {
                            let timer = Timer::start();
                            let result = store.prepare_operation(&operation, context).map_err(Error::from);
                            timer.stop(Phase::Prepare);
                            result
                        };
                        failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                        Event::Prepared { operation: operation.id, result }
                    }
                    Job::Release { id } => {
                        let result = if failed {
                            Err(Error::CommitFailed)
                        } else {
                            store.release_deployment(&id).map_err(Error::from)
                        };
                        failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                        Event::Released { result }
                    }
                    Job::Activate { deployment } => {
                        let result = if failed {
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
                        failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                        Event::Activated { result }
                    }
                    Job::Scheduling { command } => {
                        let result = if failed {
                            Err(Error::CommitFailed)
                        } else {
                            store.job_command(command.clone()).map_err(Error::from)
                        };
                        failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                        Event::Scheduled { command, result }
                    }
                    Job::Commit { expected, operation, writes, result, intents } => {
                        let id = operation.id.clone();
                        let result = if failed {
                            Err(Error::CommitFailed)
                        } else {
                            let timer = Timer::start();
                            let result = commit(store.as_mut(), expected, operation, writes, result, intents);
                            timer.stop(Phase::Commit);
                            result
                        };
                        // A later batch may depend on the failed batch's speculative
                        // writes. Never persist that suffix after an ambiguous failure.
                        failed |= result.as_ref().is_err_and(|error| !error.is_rejected_commit());
                        Event::Committed { operation: id, result }
                    }
                };
                if events.blocking_send(event).is_err() {
                    break;
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

fn commit(
    store: &mut dyn Storage,
    expected: Revision,
    operation: Operation,
    writes: Vec<Write>,
    json: Arc<str>,
    intents: Vec<chunk_store::JobIntent>,
) -> Result<(Update, Snapshot, Option<chunk_store::Jobs>)> {
    // Storage currently takes Value; only the durable boundary decodes results.
    let has_jobs = !intents.is_empty();
    let outcome = store
        .commit_with_jobs(Commit { expected, operation, writes, result: serde_json::from_str(&json)? }, intents)?;
    if expected.0.checked_add(1) != Some(outcome.revision.0) {
        return Err(Error::CommitFailed);
    }
    let snapshot = store.snapshot().map_err(|_| Error::CommitFailed)?;
    if snapshot.revision != outcome.revision {
        return Err(Error::CommitFailed);
    }
    let jobs = if has_jobs { Some(store.jobs().map_err(|_| Error::CommitFailed)?) } else { None };
    Ok((Update { revision: outcome.revision, json }, snapshot, jobs))
}

impl Drop for Committer {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
