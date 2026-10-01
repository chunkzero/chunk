use super::{Actor, MAX_DEPLOYMENTS};
use crate::{Error, Result, commit::Job, reads::View, service::Request};
use chunk_contract::Deployment;
use chunk_js::{DeploymentId, Limits};
use chunk_store::{IndexDefinition, PendingWork, Snapshot};
use std::sync::Arc;

impl Actor {
    /// Whether `snapshot` has built every index `deployment` declares, once its tables and fields are installed.
    pub(super) fn schema_ready(deployment: &Deployment, snapshot: &Snapshot) -> Result<bool> {
        for (name, table) in &deployment.tables {
            let current = snapshot.schema().get(name).ok_or(Error::Contract)?;
            if table.fields.iter().any(|(name, field)| current.fields.get(name) != Some(field)) {
                return Err(Error::Contract);
            }
        }
        Ok(IndexDefinition::declared(&deployment.tables).all(|index| snapshot.indexes().contains(&index)))
    }

    /// Installs `deployment`, unless it is resident and ready. Returns whether it started.
    pub(super) fn start_install(&mut self, deployment: &Arc<Deployment>) -> Result<bool> {
        let id = DeploymentId::new(&deployment.id)?;
        if self.releasing.as_ref().is_some_and(|(retiring, _)| retiring == &id) {
            return Err(Error::Busy);
        }
        let resident = match self.versions.get(&id) {
            Some(existing) if existing.as_deref() != Some(deployment.as_ref()) => return Err(Error::Contract),
            Some(_) if !self.unready.contains(&id) => return Ok(false),
            existing => existing.is_some(),
        };
        if self.outstanding != 0
            || self.deploying.is_some()
            || self.releasing.is_some()
            || (!resident && self.versions.len() >= MAX_DEPLOYMENTS)
        {
            return Err(Error::Busy);
        }
        if !resident {
            let env = self.actions.effects.env(deployment);
            self.js.register_with_env(id.clone(), deployment.source.clone(), Limits::default(), env.clone())?;
            let secrets = self.actions.effects.secrets.clone();
            let source =
                super::readers::Source { code: deployment.source.clone(), limits: Limits::default(), env, secrets };
            self.sources.insert(id.clone(), Arc::new(source));
        }
        if let Err(error) = self.send(Job::Install { deployment: deployment.clone() }) {
            if !resident {
                self.sources.remove(&id);
                self.js.release(&id);
            }
            return Err(error);
        }
        self.work.failed.clear();
        Ok(true)
    }

    pub(super) fn installed(&mut self, result: Result<(Snapshot, Vec<PendingWork>)>) {
        let Some((deployment, reply)) = self.deploying.take() else {
            return;
        };
        let id = DeploymentId::new(&deployment.id).expect("validated deployment");
        let resident = self.versions.contains_key(&id);
        let installed = result.and_then(|(snapshot, pending)| {
            let ready = Self::schema_ready(&deployment, &snapshot)?;
            Ok((snapshot, pending, ready))
        });
        match installed {
            Ok((snapshot, pending, ready)) => {
                self.view = Arc::new(View::new(snapshot));
                self.work.pending = pending;
                self.versions.insert(id.clone(), Some(deployment));
                if ready {
                    self.unready.remove(&id);
                } else {
                    self.unready.insert(id);
                }
                // Installing is a revision barrier; replace older reevaluation work.
                self.watches.barrier(self.view.base.revision);
                reply.finish(Ok(()));
            }
            Err(error) => {
                if !resident {
                    self.js.release(&id);
                    self.sources.remove(&id);
                }
                if !error.is_rejected_commit() {
                    self.fail(&Error::CommitFailed);
                }
                let error = match error {
                    Error::Storage(ref inner) if matches!(inner.as_ref(), chunk_store::Error::Invalid(_)) => {
                        Error::Contract
                    }
                    error => error,
                };
                reply.finish(Err(error));
            }
        }
    }

    pub(super) fn start_release(&mut self, id: DeploymentId, reply: Request<bool>) {
        if reply.cancellation.is_cancelled() {
            reply.finish(Err(Error::Cancelled));
            return;
        }
        self.watches.sweep();
        if self.outstanding != 0
            || self.deploying.is_some()
            || self.releasing.is_some()
            || self.watches.references(&id)
            || self.readers.references(&id)
            || self.reads.iter().any(|waiting| waiting.references(&id))
            || self.mutations.values().any(|m| m.call.deployment == id)
            || self.actions.references(&id)
            || self.scheduled.references(&id)
        {
            reply.finish(Err(Error::Busy));
            return;
        }
        if !self.versions.contains_key(&id) {
            reply.finish(Ok(false));
            return;
        }
        if let Err(error) = self.send(Job::Release { id: id.as_str().into() }) {
            reply.finish(Err(error));
            return;
        }
        self.releasing = Some((id, reply));
    }

    pub(super) fn released(&mut self, result: Result<(bool, Vec<PendingWork>)>) {
        let Some((id, reply)) = self.releasing.take() else {
            return;
        };
        match result {
            Ok((_, pending)) => {
                self.work.pending = pending;
                self.unready.remove(&id);
                self.work.finish(|waiting, _| (waiting == &id).then_some(Err(Error::Unknown)));
                self.versions.remove(&id);
                self.sources.remove(&id);
                self.readers.release(&id);
                reply.finish(Ok(self.js.release(&id)));
            }
            Err(error) => {
                if !error.is_rejected_commit() {
                    self.fail(&Error::CommitFailed);
                }
                reply.finish(Err(error));
            }
        }
    }
}
