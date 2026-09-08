use std::{
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

use chunk_store::{Commit, Operation, Revision, Snapshot, Storage, Write};
use tokio::sync::mpsc::Sender;

use crate::{
    Error, Result,
    service::{Event, Update},
};

pub(crate) enum Job {
    Lookup(Operation),
    Commit {
        expected: Revision,
        operation: Operation,
        writes: Vec<Write>,
        result: Arc<str>,
    },
}

pub(crate) struct Committer {
    jobs: Option<mpsc::SyncSender<Job>>,
    thread: Option<JoinHandle<()>>,
}

impl Committer {
    pub fn new(mut store: Box<dyn Storage>, events: Sender<Event>) -> Result<(Self, Snapshot)> {
        let (jobs, incoming) = mpsc::sync_channel::<Job>(64);
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("chunk-commit".into()).spawn(move || {
            if ready.send(store.snapshot()).is_err() {
                return;
            }
            let mut failed = false;
            while let Ok(job) = incoming.recv() {
                let event = match job {
                    Job::Lookup(operation) => {
                        let result = if failed {
                            Err(Error::CommitFailed)
                        } else {
                            store.outcome(&operation).map_err(Error::from).and_then(|outcome| {
                                outcome
                                    .map(|outcome| {
                                        Ok(Update {
                                            revision: outcome.revision,
                                            json: serde_json::to_string(&outcome.result)?.into(),
                                        })
                                    })
                                    .transpose()
                            })
                        };
                        Event::Lookup {
                            operation: operation.id,
                            result,
                        }
                    }
                    Job::Commit {
                        expected,
                        operation,
                        writes,
                        result,
                    } => {
                        let id = operation.id.clone();
                        let result = if failed {
                            Err(Error::CommitFailed)
                        } else {
                            commit(store.as_mut(), expected, operation, writes, result)
                        };
                        // A later batch may depend on the failed batch's speculative
                        // writes. Never persist that suffix after an ambiguous failure.
                        failed |= result.is_err();
                        Event::Committed { operation: id, result }
                    }
                };
                if events.blocking_send(event).is_err() {
                    break;
                }
            }
        })?;
        let committer = Self {
            jobs: Some(jobs),
            thread: Some(thread),
        };
        let snapshot = initialized.recv().map_err(|_| Error::Closed)??;
        Ok((committer, snapshot))
    }

    pub fn send(&self, job: Job) -> Result<()> {
        self.jobs
            .as_ref()
            .ok_or(Error::Closed)?
            .try_send(job)
            .map_err(|error| match error {
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
) -> Result<(Update, Snapshot)> {
    // Storage currently takes Value; only the durable boundary decodes results.
    let outcome = store.commit(Commit {
        expected,
        operation,
        writes,
        result: serde_json::from_str(&json)?,
    })?;
    if expected.0.checked_add(1) != Some(outcome.revision.0) {
        return Err(Error::CommitFailed);
    }
    let snapshot = store.snapshot()?;
    if snapshot.revision != outcome.revision {
        return Err(Error::CommitFailed);
    }
    Ok((
        Update {
            revision: outcome.revision,
            json,
        },
        snapshot,
    ))
}

impl Drop for Committer {
    fn drop(&mut self) {
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
