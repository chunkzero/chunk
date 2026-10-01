use std::{collections::BTreeSet, sync::Arc};

use chunk_js::{Cancellation, DeploymentId};
use chunk_store::{
    DatabaseSchema, Document, DocumentKey, IndexDefinition, IndexRange, KeyRange, Operation, Outcome, ReadBudget,
    Revision, Snapshot, SnapshotReader,
};
use serde_json::json;
use tokio::sync::{Semaphore, oneshot};

use super::{RECENT_BYTES, Watches};
use crate::{
    Error,
    limits::Limit,
    reads::{Change, Dependencies, View},
    service::{Call, GroupSubscription, Request},
};

struct Empty(DatabaseSchema, BTreeSet<IndexDefinition>);

impl SnapshotReader for Empty {
    fn outcome(&self, _: &Operation) -> chunk_store::Result<Option<Outcome>> {
        Ok(None)
    }
    fn schema(&self) -> &DatabaseSchema {
        &self.0
    }
    fn indexes(&self) -> &BTreeSet<IndexDefinition> {
        &self.1
    }
    fn get(&self, _: &DocumentKey, _: &mut ReadBudget) -> chunk_store::Result<Option<Document>> {
        Ok(None)
    }
    fn scan(&self, _: &KeyRange, _: &mut ReadBudget) -> chunk_store::Result<Vec<(String, Document)>> {
        Ok(Vec::new())
    }
    fn scan_index(&self, _: &IndexRange, _: &mut ReadBudget) -> chunk_store::Result<Vec<(String, Document)>> {
        Ok(Vec::new())
    }
}

fn view(revision: u64) -> Arc<View> {
    Arc::new(View::new(Snapshot::new(Revision(revision), Empty(DatabaseSchema::default(), BTreeSet::new()))))
}

fn key() -> DocumentKey {
    DocumentKey::new("profiles", "p").unwrap()
}

fn reads() -> Dependencies {
    Dependencies { points: [key()].into(), ..Dependencies::default() }
}

fn subscribe(watches: &mut Watches) -> oneshot::Receiver<crate::Result<GroupSubscription>> {
    subscribe_arguments(watches, json!({}))
}

fn subscribe_arguments(
    watches: &mut Watches,
    arguments: serde_json::Value,
) -> oneshot::Receiver<crate::Result<GroupSubscription>> {
    let (sender, receiver) = oneshot::channel();
    let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
    let call = Call {
        deployment: DeploymentId::new("build").unwrap(),
        function: "get".into(),
        arguments: arguments.into(),
        caller: json!({}).into(),
    };
    watches.subscribe(vec![call], Request::new(Cancellation::default(), sender, permit));
    receiver
}

fn change() -> Arc<[Change]> {
    vec![Change { key: key(), before: None, after: Some(json!({"coins": 1})) }].into()
}

#[tokio::test]
async fn removing_a_queued_batch_tail_does_not_strand_the_next_rerun() {
    let mut watches = Watches::new(Revision(0));
    let first = subscribe(&mut watches);
    let initial = watches.next_job(|| view(0)).unwrap();
    watches.complete(&initial, Ok("0".into()), reads());
    let mut group = first.await.unwrap().unwrap();
    assert_eq!(group.next().await.unwrap().results[0].as_deref().unwrap(), b"0");
    let second = subscribe_arguments(&mut watches, json!({"other": true}));
    let tail = watches.next_job(|| view(0)).unwrap();
    watches.complete(&tail, Ok("0".into()), reads());
    let cancelled = second.await.unwrap().unwrap();

    watches.changed(Revision(1), change(), 64);
    // One reader: A runs while B remains queued behind it.
    let running = watches.next_job(|| view(1)).unwrap();
    assert_eq!(running.id, initial.id);
    assert_eq!(running.view.revision, Revision(1));
    watches.changed(Revision(2), change(), 64);
    drop(cancelled);
    // Admission sweeps cancelled groups; attaching to A adds no new evaluation.
    watches.sweep();
    let _attached = subscribe(&mut watches);
    watches.complete(&running, Ok("1".into()), reads());
    let update = group.next().await.unwrap();
    assert_eq!((update.revision, update.results[0].as_deref().unwrap()), (Revision(1), &b"1"[..]));
    let mut next = Box::pin(group.next());
    crate::tests::pending(next.as_mut()).await;

    // Dispatch stops on None, so this must advance without a second call or another event.
    let rerun =
        watches.next_job(|| view(2)).expect("revision 2 rerun was stranded after the cancelled batch tail was removed");
    assert_eq!(rerun.id, initial.id);
    assert_eq!(rerun.view.revision, Revision(2));
    watches.complete(&rerun, Ok("2".into()), reads());
    let update = std::future::poll_fn(|cx| match next.as_mut().poll(cx) {
        std::task::Poll::Ready(result) => std::task::Poll::Ready(result.unwrap()),
        std::task::Poll::Pending => panic!("revision 2 rerun completed without publishing its update"),
    })
    .await;
    assert_eq!((update.revision, update.results[0].as_deref().unwrap()), (Revision(2), &b"2"[..]));
}

#[tokio::test]
async fn committed_reruns_get_a_turn_under_continuous_subscription_churn() {
    let mut watches = Watches::with_budget(Revision(0), 4096);
    let receiver = subscribe(&mut watches);
    let initial = watches.next_job(|| view(0)).unwrap();
    watches.complete(&initial, Ok("0".into()), reads());
    let mut group = receiver.await.unwrap().unwrap();
    assert_eq!(group.next().await.unwrap().results[0].as_deref().unwrap(), b"0");
    let mut next = Box::pin(group.next());
    crate::tests::pending(next.as_mut()).await;

    let mut newcomer = subscribe_arguments(&mut watches, json!({"newcomer": 0}));
    let mut running = watches.next_job(|| view(0)).unwrap();
    watches.changed(Revision(1), change(), 64);
    // Keep a replacement queued before each completion, with only one evaluation in flight.
    for index in 1..=100 {
        watches.sweep();
        let replacement = subscribe_arguments(&mut watches, json!({"newcomer": index}));
        assert!(watches.bytes < 4096, "churn must stay below the subscription memory budget");
        watches.complete(&running, Ok("0".into()), Dependencies::default());
        drop(newcomer.await.unwrap().unwrap());
        newcomer = replacement;
        watches.sweep();
        running = watches.next_job(|| view(1)).expect("subscription work remains queued");
        if running.id == initial.id {
            assert_eq!(running.view.revision, Revision(1));
            watches.complete(&running, Ok("1".into()), reads());
            let update = std::future::poll_fn(|cx| match next.as_mut().poll(cx) {
                std::task::Poll::Ready(result) => std::task::Poll::Ready(result.unwrap()),
                std::task::Poll::Pending => panic!("committed rerun completed without publishing its update"),
            })
            .await;
            assert_eq!((update.revision, update.results[0].as_deref().unwrap()), (Revision(1), &b"1"[..]));
            return;
        }
        crate::tests::pending(next.as_mut()).await;
    }
    panic!("committed rerun received no turn across 100 newcomer completions below the subscription memory budget");
}

#[tokio::test]
async fn evaluations_that_overlap_a_commit_to_their_new_reads_run_again() {
    // A commit too large to log in full is still caught through its table.
    for bytes in [64, RECENT_BYTES] {
        overlapping_commit(bytes).await;
    }
}

async fn overlapping_commit(bytes: usize) {
    let mut watches = Watches::new(Revision(1));
    let receiver = subscribe(&mut watches);
    let job = watches.next_job(|| view(1)).unwrap();
    // The index has not seen this query's reads yet, so the commit alone cannot mark it.
    watches.changed(Revision(2), change(), bytes);
    watches.complete(&job, Ok("0".into()), reads());
    let mut group: GroupSubscription = receiver.await.unwrap().unwrap();
    let update = group.next().await.unwrap();
    assert_eq!(update.revision, Revision(1), "the stale result only holds before the commit");
    let job = watches.next_job(|| view(2)).expect("the query runs again at the commit");
    watches.complete(&job, Ok("1".into()), reads());
    let update = group.next().await.unwrap();
    assert_eq!((update.revision, update.results[0].as_deref().unwrap()), (Revision(2), &b"1"[..]));
    assert!(watches.next_job(|| view(2)).is_none());
}

#[tokio::test]
async fn subscriptions_that_outgrow_the_budget_after_admission_are_closed() {
    let large = "x".repeat(4096);
    let error = Error::JavaScript(Arc::new(chunk_js::Error::JavaScript(large.clone())));
    let keys = (0..64).map(|id| DocumentKey::new("profiles", id.to_string()).unwrap()).collect();
    // A larger result, a larger error, then many reads behind an unchanged result.
    let growth = [
        (Ok(large.into()), reads()),
        (Err(error), reads()),
        (Ok("0".into()), Dependencies { points: keys, ..Dependencies::default() }),
    ];
    for (result, grown) in growth {
        let mut watches = Watches::with_budget(Revision(1), 4096);
        let receiver = subscribe(&mut watches);
        let job = watches.next_job(|| view(1)).unwrap();
        watches.complete(&job, Ok("0".into()), reads());
        let mut group = receiver.await.unwrap().unwrap();
        assert_eq!(group.next().await.unwrap().results[0].as_deref().unwrap(), b"0");
        watches.changed(Revision(2), change(), 64);
        let job = watches.next_job(|| view(2)).unwrap();
        watches.complete(&job, result, grown);
        assert!(matches!(group.next().await, Err(Error::Overloaded(Limit::SubscriptionMemory))));
        assert!(watches.next_job(|| view(2)).is_none());
        assert_eq!(watches.bytes, 0);
    }
}

#[tokio::test]
async fn withheld_results_publish_once_their_revision_is_durable() {
    let mut watches = Watches::new(Revision(0));
    let receiver = subscribe(&mut watches);
    let initial = watches.next_job(|| view(0)).unwrap();
    watches.complete(&initial, Ok("0".into()), reads());
    let mut group = receiver.await.unwrap().unwrap();
    assert_eq!(group.next().await.unwrap().results[0].as_deref().unwrap(), b"0");

    // Ack 1 carries the shared write's snapshot 2; its rerun beats ack 2.
    watches.changed(Revision(1), change(), 64);
    let rerun = watches.next_job(|| view(2)).unwrap();
    watches.complete(&rerun, Ok("1".into()), reads());
    let mut next = Box::pin(group.next());
    crate::tests::pending(next.as_mut()).await;
    watches.changed(
        Revision(2),
        vec![Change {
            key: DocumentKey::new("profiles", "q").unwrap(),
            before: None,
            after: Some(json!({"coins": 1})),
        }]
        .into(),
        64,
    );
    // Allow a fix to either publish the retained result or schedule another evaluation.
    if let Some(job) = watches.next_job(|| view(2)) {
        watches.complete(&job, Ok("1".into()), reads());
    }
    let update = std::future::poll_fn(|cx| match next.as_mut().poll(cx) {
        std::task::Poll::Ready(result) => std::task::Poll::Ready(result.unwrap()),
        std::task::Poll::Pending => panic!("result evaluated at revision 2 remained unpublished after ack 2"),
    })
    .await;
    assert_eq!((update.revision, update.results[0].as_deref().unwrap()), (Revision(2), &b"1"[..]));
}

#[test]
fn subscription_reruns_get_a_turn_under_continuous_foreground_queries() {
    use super::super::{Actor, readers::Ticket};
    use crate::service::{Command, Event};

    fn request<T>() -> (Request<T>, oneshot::Receiver<crate::Result<T>>) {
        let (sender, receiver) = oneshot::channel();
        let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
        (Request::new(Cancellation::default(), sender, permit), receiver)
    }
    fn evaluated(incoming: &mut tokio::sync::mpsc::Receiver<Event>) -> super::super::Evaluated {
        let Event::Evaluated(result) = incoming.blocking_recv().unwrap() else { panic!("expected evaluation") };
        *result
    }

    chunk_js::Engine::init_platform();
    let directory = tempfile::tempdir().unwrap();
    let (events, mut incoming) = tokio::sync::mpsc::channel(64);
    let mut actor = Actor::new(
        Box::new(crate::tests::open(&directory)),
        events,
        crate::ActionEffects::new("test".into()).unwrap(),
        crate::limits::ACTION_BYTES,
        1,
        Arc::default(),
        Arc::new(Semaphore::new(crate::limits::REQUEST_BYTES)),
    )
    .unwrap();
    let (reply, registered) = request();
    actor.request(Command::Register {
        id: DeploymentId::new("build").unwrap(),
        source: crate::tests::SOURCE.into(),
        limits: chunk_js::Limits::default(),
        reply,
    });
    registered.blocking_recv().unwrap().unwrap();
    let call = crate::tests::call("get", json!({"id": "p"}));
    let (reply, subscribed) = request();
    actor.subscribe(vec![call.clone()], reply);
    actor.dispatch();
    actor.evaluated(evaluated(&mut incoming));
    let _group = subscribed.blocking_recv().unwrap().unwrap();

    // Keep two fresh foreground requests queued at every scheduling decision.
    let mut replies = std::collections::VecDeque::new();
    for _ in 0..2 {
        let (reply, response) = request();
        actor.query(call.clone(), reply);
        replies.push_back(response);
    }
    let (reply, committed) = request();
    actor.request(Command::Mutate {
        operation: "change-p".into(),
        call: crate::tests::call("bump", json!({"id": "p"})),
        reply,
    });
    let Event::Prepared { operation, result } = incoming.blocking_recv().unwrap() else {
        panic!("expected preparation");
    };
    actor.outstanding -= 1;
    actor.prepared(&operation, result);
    let Event::Committed { operation, result } = incoming.blocking_recv().unwrap() else {
        panic!("expected commit");
    };
    actor.outstanding -= 1;
    actor.committed(&operation, result);
    assert_eq!(committed.blocking_recv().unwrap().unwrap().json.as_ref(), "1");
    for _ in 0..32 {
        actor.dispatch();
        let result = evaluated(&mut incoming);
        let rerun = matches!(result.read.ticket, Ticket::Watch(_));
        if rerun {
            assert_eq!(result.result.as_deref().unwrap(), "1");
        }
        actor.evaluated(result);
        if rerun {
            return;
        }
        replies.pop_front().unwrap().blocking_recv().unwrap().expect("foreground query must not be overloaded");
        assert!(actor.admit_read().is_ok(), "fixture must keep foreground waits below the overload threshold");
        let (reply, response) = request();
        actor.query(call.clone(), reply);
        replies.push_back(response);
    }
    panic!("subscription rerun received no turn across 32 foreground completions, with no overload signalled");
}

#[tokio::test]
async fn position_advances_follow_where_published_results_hold() {
    let mut watches = Watches::new(Revision(0));
    let receiver = subscribe(&mut watches);
    let initial = watches.next_job(|| view(0)).unwrap();
    watches.complete(&initial, Ok("0".into()), reads());
    let mut group = receiver.await.unwrap().unwrap();
    let version = group.next().await.unwrap().version;
    let mut progress = group.progress();

    // Revisions 1 and 2 share one durable write, which changes the value to 1 and back to 0; the rerun sees
    // revision 2 before revision 1 is acknowledged.
    watches.changed(Revision(1), change(), 64);
    let rerun = watches.next_job(|| view(2)).unwrap();
    watches.complete(&rerun, Ok("0".into()), reads());
    assert_eq!(progress.holds(version), None, "the published value did not hold at revision 1");

    // The unchanged rerun publishes nothing, yet the result reaches revision 2 once it is durable.
    watches.changed(Revision(2), Arc::from([]), 0);
    assert_eq!(progress.holds(version), Some(Revision(2)));
    crate::tests::pending(Box::pin(group.next()).as_mut()).await;
}
