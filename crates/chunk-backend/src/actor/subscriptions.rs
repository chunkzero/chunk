use super::Actor;
use crate::{
    limits::{Limit, SUBSCRIPTION_BYTES},
    service::{Call, GroupSubscription, Request},
};
use chunk_js::Mode;

impl Actor {
    pub(super) fn subscribe(&mut self, calls: Vec<Call>, reply: Request<GroupSubscription>) {
        self.watches.sweep();
        if self.watches.bytes() + super::watches::cost(&calls) > SUBSCRIPTION_BYTES {
            reply.finish(Err(Limit::SubscriptionMemory.exceeded()));
            return;
        }
        if let Err(error) = self.admit_read() {
            reply.finish(Err(error));
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
