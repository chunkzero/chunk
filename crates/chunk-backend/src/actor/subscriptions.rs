use super::{Actor, MAX_SUBSCRIPTIONS};
use crate::{
    Error,
    service::{Call, GroupSubscription, Request},
};
use chunk_js::Mode;

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
}
