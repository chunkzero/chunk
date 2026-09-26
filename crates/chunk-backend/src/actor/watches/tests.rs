use std::sync::Arc;

use chunk_js::{Cancellation, DeploymentId};
use chunk_store::{
    DatabaseSchema, Document, DocumentKey, IndexRange, KeyRange, Operation, Outcome, ReadBudget, Revision, Snapshot,
    SnapshotReader,
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

struct Empty(DatabaseSchema);

impl SnapshotReader for Empty {
    fn outcome(&self, _: &Operation) -> chunk_store::Result<Option<Outcome>> {
        Ok(None)
    }
    fn schema(&self) -> &DatabaseSchema {
        &self.0
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
    Arc::new(View::new(Snapshot::new(Revision(revision), Empty(DatabaseSchema::default()))))
}

fn key() -> DocumentKey {
    DocumentKey::new("profiles", "p").unwrap()
}

fn reads() -> Dependencies {
    Dependencies { points: [key()].into(), ..Dependencies::default() }
}

fn subscribe(watches: &mut Watches) -> oneshot::Receiver<crate::Result<GroupSubscription>> {
    let (sender, receiver) = oneshot::channel();
    let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
    let call = Call {
        deployment: DeploymentId::new("build").unwrap(),
        function: "get".into(),
        arguments: json!({}).into(),
        caller: json!({}).into(),
    };
    watches.subscribe(vec![call], Request::new(Cancellation::default(), sender, permit));
    receiver
}

fn change() -> Arc<[Change]> {
    vec![Change { key: key(), before: None, after: Some(json!({"coins": 1})) }].into()
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
    assert_eq!((update.revision, update.results[0].as_deref().unwrap()), (Revision(2), "1"));
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
        assert_eq!(group.next().await.unwrap().results[0].as_deref().unwrap(), "0");
        watches.changed(Revision(2), change(), 64);
        let job = watches.next_job(|| view(2)).unwrap();
        watches.complete(&job, result, grown);
        assert!(matches!(group.next().await, Err(Error::Overloaded(Limit::SubscriptionMemory))));
        assert!(watches.next_job(|| view(2)).is_none());
        assert_eq!(watches.bytes, 0);
    }
}
