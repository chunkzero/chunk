use std::{sync::Arc, time::Instant};

use chunk_contract::Function;
use chunk_js::Mode;

use super::{
    Actor,
    readers::{Evaluated, Read, Ticket},
};
use crate::{
    Error, Result,
    limits::{Limit, QUEUE_WAIT},
    reads::{Dependencies, View},
    service::{Call, Request, Update},
    timing::Phase,
};

/// A query waiting for a read engine.
pub(super) struct Waiting {
    call: Call,
    function: Option<Function>,
    reply: Request<Update>,
    since: Instant,
}

impl Waiting {
    pub fn fail(self, error: &Error) {
        self.reply.finish(Err(error.clone()));
    }

    pub fn references(&self, deployment: &chunk_js::DeploymentId) -> bool {
        &self.call.deployment == deployment
    }
}

impl Actor {
    pub(super) fn query(&mut self, call: Call, reply: Request<Update>) {
        match self.admit_read().and_then(|()| self.resolve(&call, Mode::Query)) {
            Ok(function) => self.reads.push_back(Waiting { call, function, reply, since: Instant::now() }),
            Err(error) => reply.finish(Err(error)),
        }
    }

    /// Refuses new reads while queued queries wait too long for a read engine.
    pub(super) fn admit_read(&self) -> Result<()> {
        if self.reads.front().is_some_and(|waiting| waiting.since.elapsed() > QUEUE_WAIT) {
            return Err(Limit::ReadQueue.exceeded());
        }
        Ok(())
    }

    /// Hands queued queries, then subscription reevaluations, to idle read engines.
    pub(super) fn dispatch(&mut self) {
        while self.readers.idle() {
            let read = if let Some(Waiting { call, function, reply, .. }) = self.reads.pop_front() {
                if reply.cancellation.is_cancelled() {
                    reply.finish(Err(Error::Cancelled));
                    continue;
                }
                reply.queued.stop(Phase::Queue);
                let cancellation = reply.cancellation.clone();
                let overlay = self.pending.iter().map(|pending| pending.changes.clone()).collect();
                let ticket = Ticket::Query { reply, epoch: self.epoch, overlay };
                self.read(ticket, call, function, self.view.clone(), cancellation)
            } else {
                let base = &self.view.base;
                let Some(job) = self.watches.next_job(|| Arc::new(View::new(base.clone()))) else {
                    break;
                };
                let (call, view) = (job.call.clone(), job.view.clone());
                match self.resolve(&call, Mode::Query) {
                    Ok(function) => self.read(Ticket::Watch(job), call, function, view, self.readers.stopping()),
                    Err(error) => {
                        self.watches.complete(&job, Err(error), Dependencies::default());
                        continue;
                    }
                }
            };
            if let Err(read) = self.readers.send(read) {
                self.answer(*read, Err(Error::Closed), Dependencies::default());
                if !self.readers.alive() {
                    self.fail(&Error::Closed);
                }
            }
        }
    }

    fn read(
        &self,
        ticket: Ticket,
        call: Call,
        function: Option<Function>,
        view: Arc<View>,
        cancellation: chunk_js::Cancellation,
    ) -> Read {
        let contract = self.versions.get(&call.deployment).cloned().flatten();
        let source = self.sources[&call.deployment].clone();
        Read { ticket, call, function, contract, source, view, cancellation }
    }

    pub(super) fn evaluated(&mut self, evaluated: Evaluated) {
        self.readers.done(evaluated.worker);
        self.answer(evaluated.read, evaluated.result, evaluated.reads);
    }

    fn answer(&mut self, read: Read, result: Result<Arc<str>>, reads: Dependencies) {
        let Read { ticket, call, function, view, .. } = read;
        let (reply, epoch, overlay) = match ticket {
            Ticket::Watch(job) => {
                self.watches.complete(&job, result, reads);
                return;
            }
            Ticket::Query { reply, epoch, overlay } => (reply, epoch, overlay),
        };
        if let Some(error) = &self.failure {
            reply.finish(Err(error.clone()));
            return;
        }
        if epoch != self.epoch {
            // Staged writes in its view were rolled back.
            self.reads.push_front(Waiting { call, function, reply, since: Instant::now() });
            return;
        }
        let json = match result {
            Ok(json) => json,
            Err(error) => {
                reply.finish(Err(error));
                return;
            }
        };
        let independent = overlay.iter().all(|changes| !reads.affected(changes));
        let update = Update { revision: if independent { view.base.revision } else { view.revision }, json };
        if update.revision <= self.view.base.revision {
            reply.finish(Ok(update));
            return;
        }
        let bytes = u32::try_from(update.json.len()).unwrap_or(u32::MAX);
        match self.memory.clone().try_acquire_many_owned(bytes) {
            Ok(permit) => {
                let mut reply = reply;
                reply.retain(permit);
                self.deferred.push_back((update, reply));
            }
            Err(_) => reply.finish(Err(Limit::RequestMemory.exceeded())),
        }
    }
}
