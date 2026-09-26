use std::sync::mpsc;

use chunk_js::Limits;
use chunk_store::{Revision, Storage};
use serde_json::json;
use tokio::sync::mpsc as signals;

use super::{ControlledStore, Notice, call, id, open, pending, value};
use crate::{Backend, Error};

/// Answered by the environment thread only after everything queued before it.
async fn barrier(backend: &Backend) {
    backend.query(call("get", json!({"id": "unrelated"}))).await.unwrap();
}

#[tokio::test]
async fn queued_commits_share_a_durable_write_without_early_acks_or_speculative_publishes() {
    let directory = tempfile::tempdir().unwrap();
    let mut inner = open(&directory);
    let base = inner.snapshot().unwrap().revision;
    let (notices, mut notice) = signals::unbounded_channel();
    let (scheduling, scheduling_gate) = mpsc::channel();
    let (batch, batch_gate) = mpsc::channel();
    let store = ControlledStore {
        scheduling: Some(scheduling_gate),
        batched: true,
        batch: Some(batch_gate),
        ..ControlledStore::new(inner, notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.register(id(), super::SOURCE.into(), Limits::default()).await.unwrap();
    let mut subscription = backend.subscribe(call("get", json!({"id": "p"}))).await.unwrap();
    assert_eq!(value(&subscription.next().await.unwrap()), json!(0));

    // A held scheduling command keeps the commit thread busy while four prepares queue.
    let mut blocker = Box::pin(backend.acknowledge_wake(0, None));
    pending(blocker.as_mut()).await;
    assert_eq!(notice.recv().await.unwrap(), Notice::Scheduling);
    let names = ["first", "second", "rejected", "fourth"];
    let mut mutations: Vec<_> =
        names.map(|name| Box::pin(backend.mutate(name.into(), call("bump", json!({"id": "p"}))))).into();
    for mutation in &mut mutations {
        pending(mutation.as_mut()).await;
    }
    barrier(&backend).await;
    scheduling.send(()).unwrap();
    assert!(blocker.await.is_err());
    assert_eq!(notice.recv().await.unwrap(), Notice::Batch(names.map(|name| format!("prepare {name}")).into()));

    // Each mutation runs on the previous one's unacknowledged writes. The first
    // commit write is held until every later commit has queued behind it.
    let Notice::Batch(mut commits) = notice.recv().await.unwrap() else { panic!("expected a batch") };
    barrier(&backend).await;
    for mutation in &mut mutations {
        pending(mutation.as_mut()).await;
    }
    let mut published = Box::pin(subscription.next());
    pending(published.as_mut()).await;
    batch.send(()).unwrap();
    if commits.len() < names.len() {
        let Notice::Batch(rest) = notice.recv().await.unwrap() else { panic!("expected a batch") };
        commits.extend(rest);
    }
    assert_eq!(commits, names.map(|name| format!("commit {name}")));

    let mut results = Vec::new();
    for mutation in mutations {
        results.push(mutation.await.map(|update| (update.revision, value(&update))));
    }
    assert_eq!(results[0].as_ref().unwrap(), &(Revision(base.0 + 1), json!(1)));
    assert_eq!(results[1].as_ref().unwrap(), &(Revision(base.0 + 2), json!(2)));
    // A rejected commit fails it and every commit staged on its writes.
    assert!(matches!(results[2], Err(Error::Retry)) && matches!(results[3], Err(Error::Retry)));

    // Subscribers only see durable states.
    let mut update = published.await.unwrap();
    while update.revision < Revision(base.0 + 2) {
        assert_eq!(value(&update), json!(1));
        update = subscription.next().await.unwrap();
    }
    assert_eq!((update.revision, value(&update)), (Revision(base.0 + 2), json!(2)));
    let mut published = Box::pin(subscription.next());
    barrier(&backend).await;
    pending(published.as_mut()).await;

    let next = backend.mutate("fifth".into(), call("bump", json!({"id": "p"}))).await.unwrap();
    assert_eq!((next.revision, value(&next)), (Revision(base.0 + 3), json!(3)));
    let update = published.await.unwrap();
    assert_eq!((update.revision, value(&update)), (next.revision, json!(3)));
    tokio::task::spawn_blocking(move || drop(backend)).await.unwrap();
}
