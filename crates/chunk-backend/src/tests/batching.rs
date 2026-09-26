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
    let largest_write = std::sync::Arc::default();
    let store = ControlledStore {
        scheduling: Some(scheduling_gate),
        largest_write: std::sync::Arc::clone(&largest_write),
        batched: true,
        batch: Some(batch_gate),
        ..ControlledStore::new(inner, notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.register(id(), super::SOURCE.into(), Limits::default()).await.unwrap();
    let mut subscription = backend.subscribe(call("get", json!({"id": "p"}))).await.unwrap();
    assert_eq!(value(&subscription.next().await.unwrap()), json!(0));

    // A held scheduling command keeps the commit thread busy while five prepares queue.
    let mut blocker = Box::pin(backend.acknowledge_wake(0, None));
    pending(blocker.as_mut()).await;
    assert_eq!(notice.recv().await.unwrap(), Notice::Scheduling);
    let names = ["first", "second", "third", "rejected", "fifth"];
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
    for (index, result) in results[..3].iter().enumerate() {
        let count = index as u64 + 1;
        assert_eq!(result.as_ref().unwrap(), &(Revision(base.0 + count), json!(count)));
    }
    // Whichever commits the held write took, the rest shared the next one.
    assert!(largest_write.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    // A rejected commit fails it and every commit staged on its writes.
    assert!(matches!(results[3], Err(Error::Retry)) && matches!(results[4], Err(Error::Retry)));

    // Subscribers only see durable states.
    let mut update = published.await.unwrap();
    while update.revision < Revision(base.0 + 3) {
        assert!(value(&update).as_u64().unwrap() < 3);
        update = subscription.next().await.unwrap();
    }
    assert_eq!((update.revision, value(&update)), (Revision(base.0 + 3), json!(3)));
    let mut published = Box::pin(subscription.next());
    barrier(&backend).await;
    pending(published.as_mut()).await;

    let next = backend.mutate("sixth".into(), call("bump", json!({"id": "p"}))).await.unwrap();
    assert_eq!((next.revision, value(&next)), (Revision(base.0 + 4), json!(4)));
    let update = published.await.unwrap();
    assert_eq!((update.revision, value(&update)), (next.revision, json!(4)));
    tokio::task::spawn_blocking(move || drop(backend)).await.unwrap();
}

#[tokio::test]
async fn system_commits_go_ahead_of_queued_app_commits() {
    let directory = tempfile::tempdir().unwrap();
    let (notices, mut notice) = signals::unbounded_channel();
    let (scheduling, scheduling_gate) = mpsc::channel();
    let (batch, batch_gate) = mpsc::channel();
    let store = ControlledStore {
        scheduling: Some(scheduling_gate),
        batched: true,
        batch: Some(batch_gate),
        ..ControlledStore::new(open(&directory), notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    // Gate senders drop before the backend, so a failed assertion cannot strand its join.
    let (scheduling, batch) = (scheduling, batch);
    backend.register(id(), super::SOURCE.into(), Limits::default()).await.unwrap();
    let system = backend.system();
    let schema =
        serde_json::from_value(json!({"chunk_claims": {"fields": {"player": {"schema": {"type": "string"}}}}}));
    let opened = tokio::task::spawn_blocking({
        let system = system.clone();
        move || system.open(schema.unwrap())
    });
    assert!(opened.await.unwrap().unwrap().schema().contains_key("chunk_claims"));

    // The first commit's write is held while later mutations queue their prepares, then a held scheduling command.
    let first = backend.mutate("a".into(), call("bump", json!({"id": "p"})));
    let mut first = Box::pin(first);
    pending(first.as_mut()).await;
    assert_eq!(notice.recv().await.unwrap(), Notice::Batch(vec!["prepare a".into()]));
    assert_eq!(notice.recv().await.unwrap(), Notice::Batch(vec!["commit a".into()]));
    let names = ["b", "c", "d", "e", "f"];
    let mut mutations: Vec<_> =
        names.map(|name| Box::pin(backend.mutate(name.into(), call("bump", json!({"id": "p"}))))).into();
    for mutation in &mut mutations {
        pending(mutation.as_mut()).await;
    }
    barrier(&backend).await;
    let mut blocker = Box::pin(backend.acknowledge_wake(0, None));
    pending(blocker.as_mut()).await;
    barrier(&backend).await;
    batch.send(()).unwrap();
    let first = first.await.unwrap();
    assert_eq!(notice.recv().await.unwrap(), Notice::Batch(names.map(|name| format!("prepare {name}")).into()));
    assert_eq!(notice.recv().await.unwrap(), Notice::Scheduling);

    // Every app commit is now staged and queued behind the held command when the system commit arrives.
    barrier(&backend).await;
    let key = chunk_store::DocumentKey::new("chunk_claims", "claim").unwrap();
    let write = chunk_store::Write { key, value: Some(json!({"player": "alex"})) };
    let committed = tokio::task::spawn_blocking(move || system.commit(vec![write]));
    while backend.system().queued() == 0 {
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    scheduling.send(()).unwrap();
    assert!(blocker.await.is_err());
    let Notice::Batch(write) = notice.recv().await.unwrap() else { panic!("expected a batch") };
    let system_revision = Revision(first.revision.0 + 1);
    assert_eq!(write[0], format!("commit chunk/1/{}", system_revision.0));
    assert_eq!(write[1..], names.map(|name| format!("commit {name}")));
    assert_eq!(committed.await.unwrap().unwrap(), system_revision);

    // Queued app commits moved past it and still commit, each on the previous one's writes.
    for (index, mutation) in mutations.into_iter().enumerate() {
        let update = mutation.await.unwrap();
        let count = index as u64 + 2;
        assert_eq!((update.revision, value(&update)), (Revision(system_revision.0 + count - 1), json!(count)));
    }
    let next = backend.mutate("g".into(), call("bump", json!({"id": "p"}))).await.unwrap();
    assert_eq!((next.revision, value(&next)), (Revision(system_revision.0 + 6), json!(7)));
    tokio::task::spawn_blocking(move || drop(backend)).await.unwrap();
}
