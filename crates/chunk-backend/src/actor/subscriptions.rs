use super::{Actor, MAX_SUBSCRIPTIONS};
use crate::{
    Error,
    reads::View,
    service::{Call, GroupSubscription, Request},
    timing::{Phase, Timer},
};
use chunk_js::{Cancellation, Mode};
use std::{rc::Rc, sync::Arc};

impl Actor {
    pub(super) fn subscribe(&mut self, calls: Vec<Call>, reply: Request<GroupSubscription>) {
        self.watches.sweep();
        if self.watches.len() >= MAX_SUBSCRIPTIONS {
            reply.finish(Err(Error::Busy));
            return;
        }
        for call in &calls {
            if let Err(error) = self.resolve(call, Mode::Query) {
                reply.finish(Err(error));
                return;
            }
        }
        self.watches.subscribe(calls, reply);
    }

    /// Evaluates one query of the current subscription batch.
    pub(super) fn reevaluate_one(&mut self) {
        let base = &self.view.base;
        let Some(job) = self.watches.next_job(|| Rc::new(View::new(base.clone()))) else {
            return;
        };
        let timer = Timer::start();
        let (result, reads) =
            self.evaluate_traced(&job.call, Mode::Query, job.view.clone(), &Cancellation::default(), None);
        timer.stop(Phase::Reevaluate);
        self.watches.complete(&job, result.map(|execution| Arc::from(execution.value)), reads);
    }
}
