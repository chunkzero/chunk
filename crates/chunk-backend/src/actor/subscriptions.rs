use super::{Actor, MAX_SUBSCRIPTIONS, Reevaluation, Subscribed};
use crate::{
    Error, Result,
    reads::{Dependencies, View},
    service::{Call, GroupSubscription, GroupUpdate, Request},
};
use chunk_js::{Cancellation, Mode};
use chunk_store::Write;
use std::{rc::Rc, sync::Arc};
use tokio::sync::watch;

impl Actor {
    pub(super) fn subscribe(&mut self, calls: Vec<Call>, reply: Request<GroupSubscription>) {
        if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
            reply.finish(Err(Error::Busy));
            return;
        }
        for call in &calls {
            if let Err(error) = self.resolve(call, Mode::Query) {
                reply.finish(Err(error));
                return;
            }
        }
        let view = Rc::new(View::new(self.view.base.clone()));
        let (results, dependencies) = self.evaluate_group(&calls, &view, &reply.cancellation);
        let (sender, receiver) = watch::channel(Ok(GroupUpdate {
            revision: self.view.base.revision,
            results: results.clone(),
        }));
        self.next_subscription += 1;
        self.subscriptions.push(Subscribed {
            id: self.next_subscription,
            calls,
            dependencies,
            results,
            sender,
        });
        reply.finish(Ok(GroupSubscription::new(receiver)));
    }

    fn evaluate_group(
        &mut self,
        calls: &[Call],
        view: &Rc<View>,
        cancellation: &Cancellation,
    ) -> (Vec<Result<Arc<str>>>, Dependencies) {
        let mut dependencies = Dependencies::default();
        let mut bytes = 0;
        let results = calls
            .iter()
            .map(|call| {
                let (result, reads) = self.evaluate_traced(call, Mode::Query, view.clone(), cancellation, None);
                dependencies.extend(reads);
                result.and_then(|execution| {
                    bytes += execution.value.len();
                    if bytes > 1024 * 1024 {
                        return Err(Error::Invalid("query group result limit"));
                    }
                    Ok(Arc::from(execution.value))
                })
            })
            .collect();
        (results, dependencies)
    }

    pub(super) fn publish(&mut self, writes: &[Write]) {
        let batch = Reevaluation {
            view: Rc::new(View::new(self.view.base.clone())),
            writes: Some(writes.to_vec()),
            ids: self.subscriptions.iter().map(|subscription| subscription.id).collect(),
        };
        if self.reevaluations.len() == 2 {
            // Slow watches coalesce to the latest durable snapshot. Reevaluating all
            // watches avoids retaining an unbounded history of invalidating writes.
            let next = self.reevaluations.back_mut().expect("queued batch");
            *next = Reevaluation { writes: None, ..batch };
        } else {
            self.reevaluations.push_back(batch);
        }
    }

    pub(super) fn reevaluate_one(&mut self) {
        let Some(batch) = self.reevaluations.front_mut() else {
            return;
        };
        let Some(id) = batch.ids.pop_front() else {
            self.reevaluations.pop_front();
            return;
        };
        let Some(index) = self.subscriptions.iter().position(|subscription| subscription.id == id) else {
            return;
        };
        if batch
            .writes
            .as_ref()
            .is_some_and(|writes| !self.subscriptions[index].dependencies.affected(writes))
        {
            return;
        }
        let view = batch.view.clone();
        let mut subscription = self.subscriptions.remove(index);
        if subscription.sender.is_closed() {
            return;
        }
        let (results, dependencies) = self.evaluate_group(&subscription.calls, &view, &Cancellation::default());
        subscription.dependencies = dependencies;
        let changed = results
            .iter()
            .zip(&subscription.results)
            .any(|(next, previous)| match (next, previous) {
                (Ok(a), Ok(b)) => a != b,
                (Err(a), Err(b)) => a.to_string() != b.to_string(),
                _ => true,
            });
        if changed {
            subscription.results = results;
            let _ = subscription.sender.send_replace(Ok(GroupUpdate {
                revision: view.revision,
                results: subscription.results.clone(),
            }));
        }
        self.subscriptions.push(subscription);
    }
}
