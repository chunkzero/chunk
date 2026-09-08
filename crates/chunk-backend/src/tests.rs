use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    task::Poll,
};

use chunk_js::{DeploymentId, Limits};
use chunk_store::{Commit, DatabaseSchema, DocumentKey, Operation, Outcome, Revision, Snapshot, SqliteStore, Storage};
use serde_json::{Value, json};
use tokio::sync::mpsc as signals;

use crate::{Backend, Call, Error, Update};

const SOURCE: &str = r"
export function get(ctx, args) { return ctx.db.get('profiles', args.id)?.coins ?? 0; }
export function bump(ctx, args) {
  const value = (ctx.db.get('profiles', args.id)?.coins ?? 0) + 1;
  ctx.db.put('profiles', args.id, {coins: value});
  return ctx.db.get('profiles', args.id).coins;
}
export function put(ctx, args) { ctx.db.put('profiles', args.id, args.value); return null; }
export function remove(ctx, args) { ctx.db.delete('profiles', args.id); return null; }
export function scan(ctx) { return ctx.db.scan('profiles', 'x', 'z'); }
export function selected(ctx) {
  const id = ctx.db.get('profiles', 'selector').selected;
  return ctx.db.get('profiles', id).coins;
}
";

fn id() -> DeploymentId {
    DeploymentId::new("build").unwrap()
}
fn call(function: &str, arguments: Value) -> Call {
    Call {
        deployment: id(),
        function: function.into(),
        arguments,
        caller: json!({"player": "alex"}),
    }
}
fn value(update: &Update) -> Value {
    serde_json::from_str(&update.json).unwrap()
}

fn open(directory: &tempfile::TempDir) -> SqliteStore {
    let mut store = SqliteStore::open(directory.path().join("data.db"), "test").unwrap();
    let schema: DatabaseSchema = serde_json::from_value(json!({"profiles": {"fields": {
        "coins": {"schema": {"type": "integer"}, "optional": true},
        "selected": {"schema": {"type": "string"}, "optional": true}
    }}}))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    store
}

async fn pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|cx| {
        assert!(
            matches!(future.as_mut().poll(cx), Poll::Pending),
            "response escaped before durability"
        );
        Poll::Ready(())
    })
    .await;
}

#[derive(Debug, PartialEq)]
enum Notice {
    Lookup,
    Commit(usize),
}

struct ControlledStore {
    inner: SqliteStore,
    first_lookup: AtomicBool,
    lookup: Mutex<mpsc::Receiver<()>>,
    commits: Vec<mpsc::Receiver<()>>,
    committed: usize,
    notices: signals::UnboundedSender<Notice>,
    ambiguous: bool,
}

impl Storage for ControlledStore {
    fn apply_schema(&mut self, schema: &DatabaseSchema) -> chunk_store::Result<Revision> {
        self.inner.apply_schema(schema)
    }
    fn snapshot(&mut self) -> chunk_store::Result<Snapshot> {
        self.inner.snapshot()
    }
    fn outcome(&self, operation: &Operation) -> chunk_store::Result<Option<Outcome>> {
        if self.first_lookup.swap(false, Ordering::SeqCst) {
            let _ = self.notices.send(Notice::Lookup);
            let _ = self.lookup.lock().unwrap().recv();
        }
        self.inner.outcome(operation)
    }
    fn commit(&mut self, commit: Commit) -> chunk_store::Result<Outcome> {
        let index = self.committed;
        self.committed += 1;
        let _ = self.notices.send(Notice::Commit(index));
        if let Some(gate) = self.commits.get(index) {
            let _ = gate.recv();
        }
        let outcome = self.inner.commit(commit)?;
        if self.ambiguous && index == 0 {
            return Err(chunk_store::Error::Io(std::io::Error::other("lost acknowledgement")));
        }
        Ok(outcome)
    }
}

struct Controls {
    lookup: mpsc::Sender<()>,
    commits: Vec<mpsc::Sender<()>>,
    notices: signals::UnboundedReceiver<Notice>,
}

// Drop gate senders before Backend so a failed assertion cannot strand its join.
struct Harness {
    controls: Controls,
    backend: Backend,
    directory: tempfile::TempDir,
    base: Revision,
}

impl Harness {
    async fn new(ambiguous: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open(&directory);
        let base = store.snapshot().unwrap().revision;
        let (lookup, lookup_rx) = mpsc::channel();
        let (commits, receivers): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::channel()).unzip();
        let (notices, receiver) = signals::unbounded_channel();
        let store = ControlledStore {
            inner: store,
            first_lookup: AtomicBool::new(true),
            lookup: Mutex::new(lookup_rx),
            commits: receivers,
            committed: 0,
            notices,
            ambiguous,
        };
        let backend = Backend::new(Box::new(store)).unwrap();
        backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
        Self {
            controls: Controls {
                lookup,
                commits,
                notices: receiver,
            },
            backend,
            directory,
            base,
        }
    }
}

#[tokio::test]
async fn durability_gates_pipeline_queries_and_subscriptions_in_commit_order() {
    let mut harness = Harness::new(false).await;
    let backend = &harness.backend;
    let mut subscription = backend.subscribe(call("get", json!({"id": "p"}))).await.unwrap();
    assert_eq!(value(&subscription.next().await.unwrap()), json!(0));
    let mut first = Box::pin(backend.mutate("first".into(), call("bump", json!({"id": "p"}))));
    pending(first.as_mut()).await;
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Lookup);
    let mut second = Box::pin(backend.mutate("second".into(), call("bump", json!({"id": "p"}))));
    pending(second.as_mut()).await;
    // The query reply proves both lookup jobs were enqueued before opening the gate.
    assert_eq!(
        value(&backend.query(call("get", json!({"id": "p"}))).await.unwrap()),
        json!(0)
    );
    harness.controls.lookup.send(()).unwrap();
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    let mut query = Box::pin(backend.query(call("get", json!({"id": "p"}))));
    pending(query.as_mut()).await;
    backend
        .register(DeploymentId::new("other").unwrap(), SOURCE.into(), Limits::default())
        .await
        .unwrap();
    pending(first.as_mut()).await;
    pending(second.as_mut()).await;
    pending(query.as_mut()).await;
    let mut next = Box::pin(subscription.next());
    pending(next.as_mut()).await;
    harness.controls.commits[0].send(()).unwrap();
    let first = first.await.unwrap();
    assert_eq!(value(&first), json!(1));
    assert_eq!(first.revision, Revision(harness.base.0 + 1));
    let published = next.await.unwrap();
    assert_eq!(value(&published), json!(1));
    assert_eq!(published.revision, first.revision);
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(1));
    pending(second.as_mut()).await;
    pending(query.as_mut()).await;
    harness.controls.commits[1].send(()).unwrap();
    let second = second.await.unwrap();
    assert_eq!(value(&second), json!(2));
    assert_eq!(second.revision, Revision(first.revision.0 + 1));
    let query = query.await.unwrap();
    assert_eq!(query.revision, second.revision);
    assert_eq!(value(&query), json!(2));
    assert_eq!(value(&subscription.next().await.unwrap()), json!(2));
}

#[tokio::test]
async fn admission_bounds_duplicate_waiters_and_cancelled_waiter_does_not_stage_twice() {
    let mut harness = Harness::new(false).await;
    let mut requests = Vec::new();
    for _ in 0..64 {
        let mut request = Box::pin(harness.backend.mutate("same".into(), call("bump", json!({"id": "p"}))));
        pending(request.as_mut()).await;
        requests.push(request);
    }
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Lookup);
    assert!(matches!(
        harness.backend.query(call("get", json!({"id": "p"}))).await,
        Err(Error::Busy)
    ));
    drop(requests.remove(0));
    harness.controls.lookup.send(()).unwrap();
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    harness.controls.commits[0].send(()).unwrap();
    for request in requests {
        let result = request.await.unwrap();
        assert_eq!(value(&result), json!(1));
        assert_eq!(result.revision, Revision(harness.base.0 + 1));
    }
    let result = harness.backend.query(call("get", json!({"id": "p"}))).await.unwrap();
    assert_eq!(value(&result), json!(1));
}

#[tokio::test]
async fn ambiguous_commit_stops_the_suffix_and_restart_recovers_once() {
    let mut harness = Harness::new(true).await;
    let mut first = Box::pin(harness.backend.mutate("first".into(), call("bump", json!({"id": "p"}))));
    pending(first.as_mut()).await;
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Lookup);
    let mut second = Box::pin(
        harness
            .backend
            .mutate("second".into(), call("bump", json!({"id": "p"}))),
    );
    pending(second.as_mut()).await;
    harness.backend.query(call("get", json!({"id": "p"}))).await.unwrap();
    harness.controls.lookup.send(()).unwrap();
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    harness
        .backend
        .register(DeploymentId::new("barrier").unwrap(), SOURCE.into(), Limits::default())
        .await
        .unwrap();
    harness.controls.commits[0].send(()).unwrap();
    assert!(matches!(first.await, Err(Error::CommitFailed)));
    assert!(matches!(second.await, Err(Error::CommitFailed)));
    assert!(matches!(
        harness.backend.query(call("get", json!({"id": "p"}))).await,
        Err(Error::CommitFailed)
    ));
    let Harness {
        controls,
        backend,
        directory,
        ..
    } = harness;
    drop(controls);
    drop(backend);
    let mut store = open(&directory);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(
        snapshot
            .get(&DocumentKey::new("profiles", "p").unwrap())
            .unwrap()
            .unwrap()
            .value,
        json!({"coins": 1})
    );
    let backend = Backend::new(Box::new(store)).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    let recovered = backend
        .mutate("first".into(), call("bump", json!({"id": "p"})))
        .await
        .unwrap();
    assert_eq!(value(&recovered), json!(1));
    let mut changed = call("bump", json!({"id": "p"}));
    changed.caller = json!({"player": "someone else"});
    assert!(
        matches!(backend.mutate("first".into(), changed).await, Err(Error::Storage(error)) if matches!(error.as_ref(), chunk_store::Error::OperationMismatch))
    );
    assert_eq!(
        value(
            &backend
                .mutate("second".into(), call("bump", json!({"id": "p"})))
                .await
                .unwrap()
        ),
        json!(2)
    );
}

#[tokio::test]
async fn subscriptions_track_empty_ranges_and_update_dependencies_when_results_match() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::new(Box::new(open(&directory))).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    for (key, document) in [
        ("a", json!({"coins": 1})),
        ("b", json!({"coins": 1})),
        ("selector", json!({"selected": "a"})),
    ] {
        backend
            .mutate(key.into(), call("put", json!({"id": key, "value": document})))
            .await
            .unwrap();
    }
    let mut selected = backend.subscribe(call("selected", json!({}))).await.unwrap();
    assert_eq!(value(&selected.next().await.unwrap()), json!(1));
    backend
        .mutate(
            "switch".into(),
            call("put", json!({"id": "selector", "value": {"selected": "b"}})),
        )
        .await
        .unwrap();
    backend
        .mutate("bump-b".into(), call("bump", json!({"id": "b"})))
        .await
        .unwrap();
    assert_eq!(value(&selected.next().await.unwrap()), json!(2));
    let mut range = backend.subscribe(call("scan", json!({}))).await.unwrap();
    assert_eq!(value(&range.next().await.unwrap()), json!([]));
    backend
        .mutate("insert".into(), call("put", json!({"id": "x", "value": {"coins": 3}})))
        .await
        .unwrap();
    assert_eq!(value(&range.next().await.unwrap()), json!([["x", {"coins": 3}]]));
    backend
        .mutate("remove".into(), call("remove", json!({"id": "x"})))
        .await
        .unwrap();
    assert_eq!(value(&range.next().await.unwrap()), json!([]));
    assert!(matches!(backend.release(id()).await, Err(Error::Busy)));
    drop(selected);
    drop(range);
    assert!(backend.release(id()).await.unwrap());
}
