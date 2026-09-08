use chunk_js::{Cancellation, Mode};
use chunk_store::{DocumentKey, Operation, Revision, Snapshot, Write};
use sha2::{Digest, Sha256};

use super::{Actor, MAX_PENDING, MAX_PENDING_BYTES, Mutation, Pending};
use crate::{
    Error, Result,
    commit::Job,
    reads::View,
    service::{Call, Request, Update},
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
            if active.operation.fingerprint == operation.fingerprint {
                active.waiters.push(reply);
            } else {
                reply.finish(Err(Error::OperationMismatch));
            }
            return;
        }
        if self.mutations.len() >= MAX_PENDING {
            reply.finish(Err(Error::Busy));
            return;
        }
        let outcome = self.view.base.outcome(&operation).map_err(Error::from);
        match outcome {
            Ok(Some(outcome)) => {
                reply.finish(
                    serde_json::to_string(&outcome.result)
                        .map(|json| Update {
                            revision: outcome.revision,
                            json: json.into(),
                        })
                        .map_err(Error::from),
                );
            }
            Err(error) => reply.finish(Err(error)),
            Ok(None) => {
                if self.recovering {
                    reply.finish(Err(Error::Busy));
                    return;
                }
                let mutation = Mutation {
                    operation,
                    call,
                    waiters: vec![reply],
                };
                match self.stage(&mutation) {
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

    fn operation(&self, id: String, call: &Call) -> Result<Operation> {
        if id.is_empty() || id.len() > 256 {
            return Err(Error::Invalid("operation identity"));
        }
        self.resolve(call, Mode::Mutation)?;
        // Identity describes the business request; a durable result survives redeployment.
        let request = serde_json::to_vec(&(
            "mutation-v2",
            &call.function,
            call.arguments.as_str(),
            call.caller.as_str(),
        ))?;
        Ok(Operation {
            id,
            fingerprint: Sha256::digest(request).into(),
        })
    }

    fn stage(&mut self, mutation: &Mutation) -> Result<()> {
        if self.deploying.is_some() {
            return Err(Error::Busy);
        }
        let cancellation = &Cancellation::default();
        let snapshot = self.view.clone();
        let seed = u64::from_be_bytes(
            Sha256::digest(mutation.operation.id.as_bytes())[..8]
                .try_into()
                .expect("digest prefix"),
        );
        let (execution, _) = self.evaluate_traced(
            &mutation.call,
            Mode::Mutation,
            snapshot.clone(),
            cancellation,
            Some(seed),
        );
        let execution = execution?;
        let writes = execution
            .writes
            .into_iter()
            .map(|write| {
                Ok(Write {
                    key: DocumentKey::new(write.key.table, write.key.id)?,
                    value: write.value,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if let Some(Some(contract)) = self.versions.get(&mutation.call.deployment) {
            for write in &writes {
                if !contract.tables.contains_key(&write.key.table) {
                    return Err(Error::Contract);
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
            .map(serde_json::to_vec)
            .collect::<std::result::Result<Vec<_>, _>>()?
            .iter()
            .map(Vec::len)
            .sum::<usize>();
        let bytes = written + execution.value.len() + old_bytes;
        if self.pending.len() >= MAX_PENDING || self.pending_bytes + bytes > MAX_PENDING_BYTES {
            return Err(Error::Busy);
        }
        if mutation.waiters.iter().all(|reply| reply.cancellation.is_cancelled()) {
            return Err(Error::Cancelled);
        }
        // Execution and validation are serialized on this thread, so no mutation
        // can change the read revision before this batch is applied.
        let revision = Revision(
            snapshot
                .revision
                .0
                .checked_add(1)
                .ok_or(Error::Invalid("revision exhausted"))?,
        );
        self.send(Job::Commit {
            expected: snapshot.revision,
            operation: mutation.operation.clone(),
            writes: writes.clone(),
            result: execution.value.into(),
        })?;
        drop(snapshot);
        std::rc::Rc::make_mut(&mut self.view).apply(revision, &writes);
        self.pending.push_back(Pending {
            operation: mutation.operation.id.clone(),
            revision,
            writes,
            changes,
            bytes,
        });
        self.pending_bytes += bytes;
        Ok(())
    }

    pub(super) fn committed(&mut self, id: &str, result: Result<(Update, Snapshot)>) {
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
        let (update, snapshot) = match result {
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
        if pending.operation != id || pending.revision != update.revision || snapshot.revision != update.revision {
            self.fail(&Error::CommitFailed);
            return;
        }
        self.pending_bytes -= pending.bytes;
        let mut view = View::new(snapshot);
        for next in &self.pending {
            view.apply(next.revision, &next.writes);
        }
        self.view = std::rc::Rc::new(view);
        if let Some(mutation) = self.mutations.remove(id) {
            for reply in mutation.waiters {
                reply.finish(Ok(update.clone()));
            }
        }
        while self
            .deferred
            .front()
            .is_some_and(|(query, _)| query.revision <= update.revision)
        {
            let (query, reply) = self.deferred.pop_front().expect("ready query");
            reply.finish(Ok(query));
        }
        self.publish(&pending.changes);
    }
}
