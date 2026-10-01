use std::collections::{BTreeMap, BTreeSet};

use chunk_js::DeploymentId;
use chunk_store::{Backfill, PendingWork, Snapshot};
use serde_json::Value;

use super::{
    Actor,
    readers::{Compute, Computed},
};
use crate::{
    Error, Readiness, Result,
    commit::{Job, Stored},
    service::Request,
};

/// Work installed deployments wait on, and the replies waiting for them to be ready.
#[derive(Default)]
pub(super) struct Work {
    pub pending: Vec<PendingWork>,
    /// Active expand migrations, whose old and new shapes writes keep in step.
    pub migrations: Vec<chunk_contract::Migration>,
    /// Expands whose old shape was dropped, which no deployment may lack.
    pub contracted: Vec<String>,
    /// The item in progress: with the commit thread, or a backfill batch with a read engine.
    running: Option<u64>,
    /// What the running backfill does next, once the commit thread or a read engine can take it.
    step: Option<Step>,
    /// Items that failed, until a deployment is installed again.
    pub failed: BTreeMap<u64, Error>,
    waiting: Vec<(DeploymentId, Request<()>)>,
}

/// A backfill batch's next step.
enum Step {
    Compute(Backfill),
    Commit(Backfill, Vec<Value>),
}

impl Actor {
    pub(super) fn await_ready(&mut self, id: DeploymentId, reply: Request<()>) {
        let Some(deployment) = self.versions.get(&id) else {
            reply.finish(Err(Error::Unknown));
            return;
        };
        if !self.unready.contains(&id) {
            reply.finish(Ok(()));
            return;
        }
        let failed = deployment.as_ref().and_then(|deployment| {
            let mut blocking = self.work.pending.iter().filter(|pending| pending.work.blocks(deployment));
            blocking.find_map(|pending| self.work.failed.get(&pending.id))
        });
        match failed {
            Some(error) => reply.finish(Err(error.clone())),
            None => self.work.waiting.push((id, reply)),
        }
    }

    pub(super) fn readiness(&self, id: &DeploymentId) -> Result<Readiness> {
        let deployment = self.versions.get(id).ok_or(Error::Unknown)?;
        let pending = deployment.as_ref().map_or_else(Vec::new, |deployment| {
            self.work.pending.iter().filter(|pending| pending.work.blocks(deployment)).cloned().collect()
        });
        Ok(Readiness { ready: !self.unready.contains(id), pending })
    }

    /// The tables `work` changes. Work on a table runs in order: a failed or waiting item holds back the rest.
    fn tables(&self, work: &chunk_store::Work) -> Vec<String> {
        match work {
            chunk_store::Work::Index(_) => Vec::new(),
            chunk_store::Work::Backfill { table, .. } => vec![table.clone()],
            chunk_store::Work::Drop { migration } => {
                let active = self.work.migrations.iter().find(|active| &active.id == migration);
                active.map_or_else(Vec::new, |active| active.tables.keys().cloned().collect())
            }
        }
    }

    /// Runs the next pending item that can run, unless one runs. A backfill runs once a resident deployment
    /// carries its migration: the commit thread reads each batch and commits the rows a read engine transformed.
    /// Work holds the commit thread but not `outstanding`, so installs and releases queue behind it rather than
    /// being refused.
    pub(super) fn dispatch_work(&mut self) {
        self.work.finish(|_, reply| reply.cancellation.is_cancelled().then_some(Err(Error::Cancelled)));
        if self.failure.is_some() {
            return;
        }
        if let Some(step) = self.work.step.take() {
            self.advance(step);
        }
        if self.work.running.is_some() {
            return;
        }
        let mut blocked = BTreeSet::new();
        let mut next = None;
        for pending in &self.work.pending {
            let tables = self.tables(&pending.work);
            let runnable = match &pending.work {
                _ if self.work.failed.contains_key(&pending.id)
                    || tables.iter().any(|table| blocked.contains(table)) =>
                {
                    None
                }
                chunk_store::Work::Backfill { migration, .. } => {
                    self.carrier(migration).map(|_| Job::ReadBackfill { id: pending.id })
                }
                _ => Some(Job::Work { id: pending.id }),
            };
            if runnable.is_some() {
                next = runnable;
                break;
            }
            blocked.extend(tables);
        }
        if let Some(job) = next {
            let id = match &job {
                Job::Work { id } | Job::ReadBackfill { id } => *id,
                _ => return,
            };
            if self.committer.send(job).is_ok() {
                self.work.running = Some(id);
            }
        }
    }

    fn advance(&mut self, step: Step) {
        let Some(id) = self.work.running else { return };
        match step {
            Step::Compute(batch) => {
                let deployment = self.carrier(&batch.migration).and_then(|carrier| DeploymentId::new(&carrier.id).ok());
                let source = deployment.as_ref().and_then(|deployment| self.sources.get(deployment)).cloned();
                let (Some(deployment), Some(source)) = (deployment, source) else {
                    // No deployment carries the migration now; the batch is read again once one does.
                    self.work.running = None;
                    return;
                };
                if let Err(compute) = self.readers.send_compute(Compute { id, batch, deployment, source }) {
                    self.work.step = Some(Step::Compute(compute.batch));
                    if !self.readers.alive() {
                        self.fail(&Error::Closed);
                    }
                }
            }
            Step::Commit(batch, outputs) => {
                let job = Job::CommitBackfill { id, batch: batch.clone(), outputs: outputs.clone() };
                match self.committer.send(job) {
                    Ok(()) => {}
                    Err(Error::Busy) => self.work.step = Some(Step::Commit(batch, outputs)),
                    Err(error) => self.fail(&error),
                }
            }
        }
    }

    pub(super) fn backfill_read(&mut self, id: u64, batch: Backfill) {
        if self.work.running == Some(id) {
            self.work.step = Some(Step::Compute(batch));
        }
    }

    pub(super) fn computed(&mut self, computed: Computed) {
        self.readers.done(computed.worker);
        if self.work.running != Some(computed.id) || self.failure.is_some() {
            return;
        }
        match computed.result {
            Ok((batch, outputs)) => self.work.step = Some(Step::Commit(batch, outputs)),
            Err(error) if error.is_rejected_commit() => self.worked(computed.id, Err(error)),
            Err(error) => self.worked(computed.id, Err(Error::Migration(error.to_string()))),
        }
    }

    pub(super) fn worked(&mut self, id: u64, result: Result<(Snapshot, Stored)>) {
        self.work.running = None;
        if self.failure.is_some() {
            return;
        }
        match result {
            Ok((snapshot, stored)) => {
                self.work.update(stored);
                self.rebase(snapshot);
                self.promote();
            }
            Err(error) if error.is_rejected_commit() => {
                tracing::warn!(%error, work = id, "pending work failed; deployments waiting on it are not ready");
                self.work.failed.insert(id, error.clone());
                let Some(work) = self.work.pending.iter().find(|pending| pending.id == id).map(|p| p.work.clone())
                else {
                    return;
                };
                let versions = &self.versions;
                self.work.finish(|id, _| {
                    let deployment = versions.get(id).cloned().flatten()?;
                    work.blocks(&deployment).then(|| Err(error.clone()))
                });
            }
            Err(_) => self.fail(&Error::CommitFailed),
        }
    }

    /// Marks deployments that no longer wait on work as ready, finishing their waiters.
    pub(super) fn promote(&mut self) {
        let (snapshot, versions, work) = (&self.view.base, &self.versions, &self.work);
        self.unready.retain(|id| {
            let deployment = versions.get(id).cloned().flatten();
            !deployment.is_some_and(|deployment| Self::schema_ready(&deployment, snapshot, work).unwrap_or(false))
        });
        let unready = &self.unready;
        self.work.finish(|id, _| (!unready.contains(id)).then_some(Ok(())));
    }
}

impl Work {
    pub fn new(stored: Stored) -> Self {
        Self { pending: stored.work, migrations: stored.migrations, contracted: stored.contracted, ..Self::default() }
    }

    pub fn update(&mut self, stored: Stored) {
        self.pending = stored.work;
        self.migrations = stored.migrations;
        self.contracted = stored.contracted;
    }

    /// Finishes each waiting reply `outcome` decides, keeping the rest.
    pub fn finish(&mut self, mut outcome: impl FnMut(&DeploymentId, &Request<()>) -> Option<Result<()>>) {
        for (id, reply) in std::mem::take(&mut self.waiting) {
            match outcome(&id, &reply) {
                Some(result) => reply.finish(result),
                None => self.waiting.push((id, reply)),
            }
        }
    }
}
