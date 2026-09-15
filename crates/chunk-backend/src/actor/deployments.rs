use super::{Actor, MAX_DEPLOYMENTS, Reevaluation};
use crate::{Error, Result, commit::Job, reads::View, service::Request};
use chunk_contract::Deployment;
use chunk_js::{DeploymentId, Limits};
use chunk_store::Snapshot;
use std::{rc::Rc, sync::Arc};

impl Actor {
    pub(super) fn schema_ready(deployment: &Deployment, installed: &chunk_contract::DatabaseSchema) -> Result<()> {
        for (name, table) in &deployment.tables {
            let current = installed.get(name).ok_or(Error::Contract)?;
            if table.fields.iter().any(|(name, field)| current.fields.get(name) != Some(field))
                || table.indexes.iter().any(|(name, fields)| current.indexes.get(name) != Some(fields))
            {
                return Err(Error::Contract);
            }
        }
        Ok(())
    }

    pub(super) fn start_deployment(&mut self, deployment: &Arc<Deployment>) -> Result<bool> {
        let id = DeploymentId::new(&deployment.id)?;
        if self.releasing.as_ref().is_some_and(|(retiring, _)| retiring == &id) {
            return Err(Error::Busy);
        }
        if let Some(existing) = self.versions.get(&id) {
            return if existing.as_deref() == Some(deployment.as_ref()) { Ok(false) } else { Err(Error::Contract) };
        }
        if self.outstanding != 0
            || self.deploying.is_some()
            || self.releasing.is_some()
            || self.versions.len() >= MAX_DEPLOYMENTS
        {
            return Err(Error::Busy);
        }
        self.js.register(id.clone(), deployment.source.clone(), Limits::default())?;
        if let Err(error) = self.send(Job::Activate { deployment: deployment.clone() }) {
            self.js.release(&id);
            return Err(error);
        }
        Ok(true)
    }

    pub(super) fn activated(&mut self, result: Result<Snapshot>) {
        let Some((deployment, reply)) = self.deploying.take() else {
            return;
        };
        let id = DeploymentId::new(&deployment.id).expect("validated deployment");
        match result {
            Ok(snapshot) => {
                self.view = Rc::new(View::new(snapshot));
                self.versions.insert(id, Some(deployment));
                // Schema activation is a revision barrier; replace older reevaluation work.
                self.reevaluations.clear();
                self.reevaluations.push_back(Reevaluation {
                    view: self.view.clone(),
                    changes: None,
                    ids: self.subscriptions.iter().map(|s| s.id).collect(),
                });
                reply.finish(Ok(()));
            }
            Err(error) => {
                self.js.release(&id);
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
        if self.outstanding != 0
            || self.deploying.is_some()
            || self.releasing.is_some()
            || self.subscriptions.iter().any(|s| s.calls.iter().any(|c| c.deployment == id))
            || self.mutations.values().any(|m| m.call.deployment == id)
            || self.actions.references(&id)
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

    pub(super) fn released(&mut self, result: Result<bool>) {
        let Some((id, reply)) = self.releasing.take() else {
            return;
        };
        match result {
            Ok(_) => {
                self.versions.remove(&id);
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
