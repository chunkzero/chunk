use chunk_js::{Cancellation, Mode};
use chunk_store::{DocumentKey, Operation, Revision, Snapshot, Write};
use sha2::{Digest, Sha256};

use super::{Actor, Mutation, Pending};
use crate::{
    Error, Result,
    commit::Job,
    limits::{Limit, MUTATION_BYTES, QUEUE_WAIT},
    reads::View,
    service::{Call, Request, Update},
    timing::{Phase, Timer},
};

impl Actor {
    pub(super) fn mutate(&mut self, id: String, call: Call, reply: Request<Update>) {
        if reply.cancellation.is_cancelled() {
            reply.finish(Err(Error::Cancelled));
            return;
        }
        let result = self.operation(id, &call);
        let operation = match result {
            Ok(operation) => operation,
            Err(error) => {
                reply.finish(Err(error));
                return;
            }
        };
        if let Some(active) = self.mutations.get_mut(&operation.id) {
            if active.operation.fingerprint == operation.fingerprint && active.call.deployment == call.deployment {
                active.waiters.push(reply);
            } else {
                reply.finish(Err(Error::OperationMismatch));
            }
            return;
        }
        if let Err(error) = self.admit_mutation(&call) {
            reply.finish(Err(error));
            return;
        }
        let outcome = self.view.base.outcome(&operation).map_err(Error::from);
        match outcome {
            Ok(Some(outcome)) => {
                reply.finish(
                    serde_json::to_string(&outcome.result)
                        .map(|json| Update { revision: outcome.revision, json: json.into() })
                        .map_err(Error::from),
                );
            }
            Err(error) => reply.finish(Err(error)),
            Ok(None) => {
                if self.recovering || self.deploying.is_some() || self.releasing.is_some() {
                    reply.finish(Err(Error::Busy));
                    return;
                }
                let admitted = std::time::Instant::now();
                let mutation = Mutation { operation, context: None, call, waiters: vec![reply], admitted };
                let context = chunk_store::RetryContext {
                    deployment: mutation.call.deployment.as_str().into(),
                    timestamp: self.view.base.timestamp,
                    seed: u64::from_be_bytes(
                        Sha256::digest(mutation.operation.id.as_bytes())[..8].try_into().expect("digest prefix"),
                    ),
                };
                match self.send_commit(Job::Prepare { operation: mutation.operation.clone(), context }) {
                    Ok(()) => {
                        self.mutations.insert(mutation.operation.id.clone(), mutation);
                    }
                    Err(error) => {
                        for reply in mutation.waiters {
                            reply.finish(Err(error.clone()));
                        }
                    }
                }
            }
        }
    }

    fn admit_mutation(&self, call: &Call) -> Result<()> {
        if self.mutations.values().any(|mutation| mutation.admitted.elapsed() > QUEUE_WAIT) {
            return Err(Limit::CommitQueue.exceeded());
        }
        let admitted: usize = self.mutations.values().map(|mutation| mutation.call.bytes()).sum();
        if self.pending_bytes + admitted + call.bytes() > MUTATION_BYTES {
            return Err(Limit::MutationMemory.exceeded());
        }
        Ok(())
    }

    /// The commit thread's queue is full only when commits fall behind.
    fn send_commit(&mut self, job: Job) -> Result<()> {
        self.send(job).map_err(|error| match error {
            Error::Busy => Limit::CommitQueue.exceeded(),
            error => error,
        })
    }

    pub(super) fn prepared(&mut self, id: &str, result: Result<chunk_store::RetryContext>) {
        if self.failure.is_some() {
            return;
        }
        if self.recovering {
            if result.as_ref().is_err_and(|error| !error.is_rejected_commit()) {
                self.fail(&Error::CommitFailed);
            }
            self.recovering = self.outstanding != 0;
            return;
        }
        let Some(mut mutation) = self.mutations.remove(id) else {
            return;
        };
        let context = match result {
            Ok(context) => context,
            Err(error) => {
                if !error.is_rejected_commit() {
                    self.fail(&Error::CommitFailed);
                }
                for reply in mutation.waiters {
                    reply.finish(Err(error.clone()));
                }
                return;
            }
        };
        mutation.context = Some(context);
        match self.stage(&mutation) {
            Ok(()) => {
                self.mutations.insert(id.into(), mutation);
            }
            Err(error) => {
                for reply in mutation.waiters {
                    reply.finish(Err(error.clone()));
                }
            }
        }
    }

    fn operation(&self, id: String, call: &Call) -> Result<Operation> {
        if id.is_empty() || id.len() > 256 {
            return Err(Error::Invalid("operation identity"));
        }
        self.resolve(call, Mode::Mutation)?;
        // Identity describes the business request; a durable result survives redeployment.
        let request =
            serde_json::to_vec(&("mutation-v2", &call.function, call.arguments.as_str(), call.caller.as_str()))?;
        Ok(Operation { id, fingerprint: Sha256::digest(request).into() })
    }

    fn stage(&mut self, mutation: &Mutation) -> Result<()> {
        if self.deploying.is_some() || self.releasing.is_some() {
            return Err(Error::Busy);
        }
        let timer = Timer::start();
        let cancellation = &Cancellation::default();
        let snapshot = self.view.clone();
        let context = mutation.context.as_ref().ok_or(Error::Invalid("operation not prepared"))?;
        let (execution, _) = self.evaluate_traced(
            &mutation.call,
            Mode::Mutation,
            snapshot.clone(),
            cancellation,
            Some((context.timestamp, context.seed, mutation.operation.id.clone())),
        );
        let execution = execution?;
        let intents = self.scheduled_intents(&mutation.call, execution.jobs, context.timestamp)?;
        let mut writes = execution
            .writes
            .into_iter()
            .map(|write| Ok(Write { key: DocumentKey::new(write.key.table, write.key.id)?, value: write.value }))
            .collect::<Result<Vec<_>>>()?;
        if let Some(Some(contract)) = self.versions.get(&mutation.call.deployment) {
            let mut budget = crate::reads::read_budget();
            for write in &mut writes {
                let table = contract.tables.get(&write.key.table).ok_or(Error::Contract)?;
                if let Some(value) = &mut write.value {
                    if !table.accepts(value) {
                        return Err(Error::Contract);
                    }
                    // Writes replace this deployment's fields, preserving fields owned by newer declarations.
                    if let Some(previous) = snapshot.get(&write.key, &mut budget)? {
                        let object = value.as_object_mut().ok_or(Error::Contract)?;
                        for (name, value) in previous.value.as_object().ok_or(Error::Contract)? {
                            if !table.fields.contains_key(name) {
                                object.insert(name.clone(), value.clone());
                            }
                        }
                    }
                }
                if let Some(value) = &write.value {
                    for retained in self.versions.values().flatten() {
                        if let Some(table) = retained.tables.get(&write.key.table) {
                            crate::reads::project(table, value)?;
                        }
                    }
                }
            }
        }
        let written = snapshot.validate(&writes)?;
        let changes = snapshot.changes(&writes)?;
        let old_bytes = changes
            .iter()
            .filter_map(|c| c.before.as_ref())
            .map(|before| serde_json::to_vec(before).map(|bytes| bytes.len()))
            .sum::<serde_json::Result<usize>>()?;
        let bytes = written + execution.value.len() + old_bytes;
        if self.pending_bytes + bytes > MUTATION_BYTES {
            return Err(Limit::MutationMemory.exceeded());
        }
        if mutation.waiters.iter().all(|reply| reply.cancellation.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        // Execution and validation are serialized on this thread, so no mutation
        // can change the read revision before this batch is applied.
        let revision = Revision(snapshot.revision.0.checked_add(1).ok_or(Error::Invalid("revision exhausted"))?);
        timer.stop(Phase::Mutation);
        self.send_commit(Job::Commit {
            expected: snapshot.revision,
            operation: mutation.operation.clone(),
            writes: writes.clone(),
            result: execution.value.into(),
            intents,
        })?;
        drop(snapshot);
        std::sync::Arc::make_mut(&mut self.view).apply(revision, &writes);
        self.pending.push_back(Pending {
            operation: mutation.operation.id.clone(),
            revision,
            writes,
            changes: changes.into(),
            bytes,
            staged: Timer::start(),
        });
        self.pending_bytes += bytes;
        Ok(())
    }

    pub(super) fn committed(&mut self, id: &str, result: Result<(Update, Snapshot, Option<chunk_store::Jobs>)>) {
        if self.failure.is_some() {
            return;
        }
        if self.recovering {
            if result.as_ref().is_err_and(|error| !error.is_rejected_commit()) {
                self.fail(&Error::CommitFailed);
            }
            self.recovering = self.outstanding != 0;
            return;
        }
        let (update, snapshot, jobs) = match result {
            Ok(value) => value,
            Err(error) if error.is_rejected_commit() => {
                self.reset_pending(&Error::Retry);
                self.recovering = self.outstanding != 0;
                return;
            }
            Err(_) => {
                self.fail(&Error::CommitFailed);
                return;
            }
        };
        let Some(pending) = self.pending.pop_front() else {
            self.fail(&Error::CommitFailed);
            return;
        };
        // A shared durable write acknowledges each of its commits with the same, later snapshot.
        if pending.operation != id || pending.revision != update.revision || snapshot.revision < update.revision {
            self.fail(&Error::CommitFailed);
            return;
        }
        pending.staged.stop(Phase::Durable);
        self.pending_bytes -= pending.bytes;
        let durable = snapshot.revision;
        let mut view = View::new(snapshot);
        for next in self.pending.iter().filter(|next| next.revision > durable) {
            view.apply(next.revision, &next.writes);
        }
        self.view = std::sync::Arc::new(view);
        if let Some(jobs) = jobs {
            self.scheduled.snapshot = jobs;
        }
        if let Some(mutation) = self.mutations.remove(id) {
            for reply in mutation.waiters {
                reply.finish(Ok(update.clone()));
            }
        }
        for (query, reply) in std::mem::take(&mut self.deferred) {
            if query.revision <= update.revision {
                reply.finish(Ok(query));
            } else {
                self.deferred.push_back((query, reply));
            }
        }
        self.watches.changed(update.revision, pending.changes, pending.bytes);
    }
}
