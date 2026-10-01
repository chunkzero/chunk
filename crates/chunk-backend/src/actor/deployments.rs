use super::{Actor, MAX_DEPLOYMENTS};
use crate::{Error, Result, commit::Job, reads::View, service::Request};
use chunk_contract::Deployment;
use chunk_js::{DeploymentId, Limits};
use chunk_store::{IndexDefinition, Snapshot};
use std::{collections::BTreeSet, sync::Arc};

impl Actor {
    /// Whether `snapshot` has built every index `deployment` declares and no pending work blocks it, once its
    /// tables and fields are installed.
    pub(super) fn schema_ready(
        deployment: &Deployment,
        snapshot: &Snapshot,
        work: &super::readiness::Work,
    ) -> Result<bool> {
        let journal = &deployment.contracts.migrations;
        if chunk_store::rolled_back_past(work.contracted.iter().map(String::as_str), journal).is_some() {
            return Err(Error::Contract);
        }
        let migrating: BTreeSet<_> = work
            .migrations
            .iter()
            .flat_map(|migration| &migration.tables)
            .flat_map(|(table, change)| change.added.iter().chain(&change.removed).map(move |field| (table, field)))
            .collect();
        for (name, table) in &deployment.tables {
            let current = snapshot.schema().get(name).ok_or(Error::Contract)?;
            let serves = |(field, declared)| {
                current.fields.get(field).is_some_and(|stored| {
                    chunk_store::compatible_field(stored, declared, migrating.contains(&(name, field)))
                })
            };
            if !table.fields.iter().all(serves) {
                return Err(Error::Contract);
            }
        }
        let blocked = work.pending.iter().any(|pending| pending.work.blocks(deployment));
        Ok(!blocked && IndexDefinition::declared(&deployment.tables).all(|index| snapshot.indexes().contains(&index)))
    }

    /// Installs `deployment`, unless it is resident and ready. Returns whether it started.
    pub(super) fn start_install(&mut self, deployment: &Arc<Deployment>) -> Result<bool> {
        let id = DeploymentId::new(&deployment.id)?;
        if self.retired.contains(&id) || self.releasing.as_ref().is_some_and(|(retiring, _)| retiring == &id) {
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
        Ok(true)
    }

    pub(super) fn installed(&mut self, result: Result<(Snapshot, crate::commit::Stored)>) {
        let Some((deployment, reply)) = self.deploying.take() else {
            return;
        };
        let id = DeploymentId::new(&deployment.id).expect("validated deployment");
        let resident = self.versions.contains_key(&id);
        let installed = result.and_then(|(snapshot, stored)| {
            self.work.update(stored);
            let ready = Self::schema_ready(&deployment, &snapshot, &self.work)?;
            Ok((snapshot, ready))
        });
        match installed {
            Ok((snapshot, ready)) => {
                self.view = Arc::new(View::new(snapshot));
                self.work.failed.clear();
                if self.versions.insert(id.clone(), Some(deployment)).is_none() {
                    self.installed.push(id.clone());
                }
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

    /// Refuses new work for `id` and closes its subscriptions, so only work already running keeps it resident. Applied
    /// once the retirement is durable.
    pub(super) fn fence(&mut self, id: &DeploymentId) {
        if self.versions.contains_key(id) {
            self.retired.insert(id.clone());
            self.watches.retire(id, &Error::Retired);
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

    pub(super) fn released(&mut self, result: Result<(bool, crate::commit::Stored)>) {
        let Some((id, reply)) = self.releasing.take() else {
            return;
        };
        match result {
            Ok((_, stored)) => {
                self.work.update(stored);
                self.unready.remove(&id);
                self.work.finish(|waiting, _| (waiting == &id).then_some(Err(Error::Unknown)));
                self.versions.remove(&id);
                self.installed.retain(|installed| installed != &id);
                self.retired.remove(&id);
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
