use std::sync::Arc;

use chunk_js::{Cancellation, DeploymentId};
use chunk_store::{
    DatabaseSchema, Document, DocumentKey, IndexRange, KeyRange, Operation, Outcome, ReadBudget, Revision, Snapshot,
    SnapshotReader,
};
use serde_json::json;
use tokio::sync::{Semaphore, oneshot};

use super::Watches;
use crate::{
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

#[tokio::test]
async fn evaluations_that_overlap_a_commit_to_their_new_reads_run_again() {
    let mut watches = Watches::new(Revision(1));
    let (sender, receiver) = oneshot::channel();
    let permit = Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap();
    let call = Call {
        deployment: DeploymentId::new("build").unwrap(),
        function: "get".into(),
        arguments: json!({}).into(),
        caller: json!({}).into(),
    };
    watches.subscribe(vec![call], Request::new(Cancellation::default(), sender, permit));
    let job = watches.next_job(|| view(1)).unwrap();
    // The index has not seen this query's reads yet, so the commit alone cannot mark it.
    let change = Change { key: key(), before: None, after: Some(json!({"coins": 1})) };
    watches.changed(Revision(2), vec![change].into());
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
