use std::collections::BTreeMap;

use chunk_js::DeploymentId;
use chunk_store::{PendingWork, Snapshot};

use super::Actor;
use crate::{Error, Readiness, Result, commit::Job, service::Request};

/// Work installed deployments wait on, and the replies waiting for them to be ready.
#[derive(Default)]
pub(super) struct Work {
    pub pending: Vec<PendingWork>,
    /// The item the commit thread runs.
    running: Option<u64>,
    /// Items that failed, until a deployment is installed again.
    pub failed: BTreeMap<u64, Error>,
    waiting: Vec<(DeploymentId, Request<()>)>,
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

    /// Runs the next pending item that has not failed, unless one runs. Builds hold the commit thread but not
    /// `outstanding`, so installs and releases queue behind them rather than being refused.
    pub(super) fn dispatch_work(&mut self) {
        self.work.finish(|_, reply| reply.cancellation.is_cancelled().then_some(Err(Error::Cancelled)));
        if self.work.running.is_some() || self.failure.is_some() {
            return;
        }
        let next = self.work.pending.iter().find(|pending| !self.work.failed.contains_key(&pending.id));
        if let Some(id) = next.map(|pending| pending.id)
            && self.committer.send(Job::Work { id }).is_ok()
        {
            self.work.running = Some(id);
        }
    }

    pub(super) fn worked(&mut self, id: u64, result: Result<(Snapshot, Vec<PendingWork>)>) {
        self.work.running = None;
        if self.failure.is_some() {
            return;
        }
        match result {
            Ok((snapshot, pending)) => {
                self.work.pending = pending;
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

    /// Marks deployments whose indexes the view has built as ready, finishing their waiters.
    pub(super) fn promote(&mut self) {
        let snapshot = &self.view.base;
        let versions = &self.versions;
        self.unready.retain(|id| {
            let deployment = versions.get(id).cloned().flatten();
            !deployment.is_some_and(|deployment| Self::schema_ready(&deployment, snapshot).unwrap_or(false))
        });
        let unready = &self.unready;
        self.work.finish(|id, _| (!unready.contains(id)).then_some(Ok(())));
    }
}

impl Work {
    pub fn new(pending: Vec<PendingWork>) -> Self {
        Self { pending, ..Self::default() }
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
