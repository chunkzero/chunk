use chunk_js::Mode;
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
        if let Err(error) = self.send(Job::Lookup(operation.clone())) {
            reply.finish(Err(error));
            return;
        }
        self.mutations.insert(
            operation.id.clone(),
            Mutation {
                operation,
                call,
                waiters: vec![reply],
            },
        );
    }

    fn operation(&self, id: String, call: &Call) -> Result<Operation> {
        if id.is_empty() || id.len() > 256 {
            return Err(Error::Invalid("operation identity"));
        }
        let source = self
            .versions
            .get(&call.deployment)
            .ok_or(Error::Invalid("unknown deployment"))?;
        let request = serde_json::to_vec(&(
            "mutation-v1",
            source,
            call.deployment.as_str(),
            &call.function,
            &call.arguments,
            &call.caller,
        ))?;
        if request.len() > 2 * 1024 * 1024 + 1024 {
            return Err(Error::Invalid("input limit"));
        }
        Ok(Operation {
            id,
            fingerprint: Sha256::digest(request).into(),
        })
    }

    pub(super) fn looked_up(&mut self, id: &str, result: Result<Option<Update>>, stopped: bool) {
        let Some(mut mutation) = self.mutations.remove(id) else {
            return;
        };
        let result = if stopped { Err(Error::Closed) } else { result };
        match result {
            Ok(None) => {
                mutation.waiters.retain(|reply| !reply.cancellation.is_cancelled());
                if mutation.waiters.is_empty() {
                    return;
                }
                match self.stage(&mutation) {
                    Ok(()) => {
                        self.mutations.insert(id.to_owned(), mutation);
                    }
                    Err(error) => {
                        for reply in mutation.waiters {
                            reply.finish(Err(error.clone()));
                        }
                    }
                }
            }
            result => {
                let result = result.map(|outcome| outcome.expect("existing outcome"));
                for reply in mutation.waiters {
                    reply.finish(result.clone());
                }
            }
        }
    }

    fn stage(&mut self, mutation: &Mutation) -> Result<()> {
        let cancellation = &mutation.waiters[0].cancellation;
        let snapshot = self.view.clone();
        let (execution, _) = self.evaluate(&mutation.call, Mode::Mutation, snapshot.clone(), cancellation)?;
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
        let bytes = snapshot.validate(&writes)? + execution.value.len();
        if self.pending.len() >= MAX_PENDING || self.pending_bytes + bytes > MAX_PENDING_BYTES {
            return Err(Error::Busy);
        }
        if cancellation.is_cancelled() {
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
            bytes,
        });
        self.pending_bytes += bytes;
        Ok(())
    }

    pub(super) fn committed(&mut self, id: &str, result: Result<(Update, Snapshot)>) {
        if self.failure.is_some() {
            return;
        }
        let Ok((update, snapshot)) = result else {
            self.fail(&Error::CommitFailed);
            return;
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
        self.publish(&pending.writes);
    }
}
