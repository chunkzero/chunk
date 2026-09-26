use std::{
    future::{Future, poll_fn},
    pin::Pin,
    sync::mpsc,
    task::Poll,
};

use chunk_js::{DeploymentId, Limits};
use chunk_store::{
    Commit, DatabaseSchema, DocumentKey, Operation, Outcome, Reply, Request, Revision, Snapshot, SqliteStore, Storage,
};
use serde_json::{Value, json};
use tokio::sync::mpsc as signals;

use crate::{Backend, Call, Error, Limit, Update};

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
export function indexed(ctx) { return ctx.db.scanIndex({table:'profiles',index:'by_coins',prefix:[],start:1,end:4,limit:1}); }
export function seedIndex(ctx) {
  for(const [id,coins] of [['a',1],['b',2],['c',3]]) ctx.db.put('profiles',id,{coins});
  return null;
}
export function shiftIndex(ctx) {
  ctx.db.delete('profiles','a'); ctx.db.put('profiles','b',{coins:4});
  return indexed(ctx);
}
";

fn id() -> DeploymentId {
    DeploymentId::new("build").unwrap()
}
fn call(function: &str, arguments: Value) -> Call {
    Call {
        deployment: id(),
        function: function.into(),
        arguments: arguments.into(),
        caller: json!({"player": "alex"}).into(),
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
    }, "indexes": {"by_coins": ["coins"]}}}))
    .unwrap();
    store.apply_schema(&schema).unwrap();
    store
}

async fn pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|cx| {
        assert!(matches!(future.as_mut().poll(cx), Poll::Pending), "response escaped before durability");
        Poll::Ready(())
    })
    .await;
}

#[derive(Debug, PartialEq)]
enum Notice {
    Preparing,
    Commit(usize),
    Scheduling,
    /// The requests of one shared durable write, as `prepare {id}` or `commit {id}`.
    Batch(Vec<String>),
}

struct ControlledStore {
    inner: SqliteStore,
    prepare: Option<mpsc::Receiver<()>>,
    commits: Vec<mpsc::Receiver<()>>,
    committed: usize,
    notices: signals::UnboundedSender<Notice>,
    ambiguous: bool,
    rejected: bool,
    attempts: Option<std::sync::Arc<std::sync::Mutex<Vec<Value>>>>,
    /// Holds the first scheduling command, keeping the commit thread busy.
    scheduling: Option<mpsc::Receiver<()>>,
    /// Sends shared writes to the inner store whole, reporting each one and
    /// rejecting the commit of operation `rejected` with `Capacity`.
    batched: bool,
    /// Holds the first shared write that contains a commit.
    batch: Option<mpsc::Receiver<()>>,
    /// The most successful commits one underlying write has persisted.
    largest_write: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl ControlledStore {
    fn new(inner: SqliteStore, notices: signals::UnboundedSender<Notice>) -> Self {
        Self {
            inner,
            prepare: None,
            commits: Vec::new(),
            committed: 0,
            notices,
            ambiguous: false,
            rejected: false,
            attempts: None,
            scheduling: None,
            batched: false,
            batch: None,
            largest_write: std::sync::Arc::default(),
        }
    }

    fn shared(&mut self, requests: Vec<Request>) -> Vec<chunk_store::Result<Reply>> {
        let names = requests
            .iter()
            .map(|request| match request {
                Request::Prepare { operation, .. } => format!("prepare {}", operation.id),
                Request::Commit { commit, .. } => format!("commit {}", commit.operation.id),
            })
            .collect();
        let _ = self.notices.send(Notice::Batch(names));
        if requests.iter().any(|request| matches!(request, Request::Commit { .. }))
            && let Some(gate) = self.batch.take()
        {
            let _ = gate.recv();
        }
        let (mut results, mut run) = (Vec::new(), Vec::new());
        for request in requests {
            if matches!(&request, Request::Commit { commit, .. } if commit.operation.id == "rejected") {
                results.extend(self.write(std::mem::take(&mut run)));
                results.push(Err(chunk_store::Error::Capacity));
            } else {
                run.push(request);
            }
        }
        results.extend(self.write(run));
        results
    }

    fn write(&mut self, requests: Vec<Request>) -> Vec<chunk_store::Result<Reply>> {
        let results = self.inner.batch(requests);
        let committed = results.iter().filter(|result| matches!(result, Ok(Reply::Committed(_)))).count();
        self.largest_write.fetch_max(committed, std::sync::atomic::Ordering::SeqCst);
        results
    }
}

impl Storage for ControlledStore {
    fn batch(&mut self, requests: Vec<Request>) -> Vec<chunk_store::Result<Reply>> {
        if self.batched {
            return self.shared(requests);
        }
        let mut results = Vec::new();
        for request in requests {
            let result = match request {
                Request::Prepare { operation, context } => {
                    self.prepare_operation(&operation, context).map(Reply::Prepared)
                }
                Request::Commit { commit, intents } => self.commit_with_jobs(commit, intents).map(Reply::Committed),
            };
            let stop = result.as_ref().is_err_and(|error| !error.rejected());
            results.push(result);
            if stop {
                break;
            }
        }
        results
    }

    fn job_command(&mut self, command: chunk_store::JobCommand) -> chunk_store::Result<chunk_store::Jobs> {
        if matches!(command, chunk_store::JobCommand::Recover) {
            return self.jobs();
        }
        if let Some(gate) = self.scheduling.take() {
            let _ = self.notices.send(Notice::Scheduling);
            let _ = gate.recv();
        }
        Err(chunk_store::Error::Invalid("durable scheduling unsupported"))
    }

    fn activate_deployment(&mut self, deployment: &chunk_contract::Deployment) -> chunk_store::Result<Revision> {
        self.inner.activate_deployment(deployment)
    }
    fn release_deployment(&mut self, id: &str) -> chunk_store::Result<bool> {
        self.inner.release_deployment(id)
    }

    fn prepare_operation(
        &mut self,
        operation: &Operation,
        context: chunk_store::RetryContext,
    ) -> chunk_store::Result<chunk_store::RetryContext> {
        if let Some(gate) = self.prepare.take() {
            let _ = self.notices.send(Notice::Preparing);
            let _ = gate.recv();
        }
        self.inner.prepare_operation(operation, context)
    }

    fn deployments(&self) -> chunk_store::Result<Vec<chunk_contract::Deployment>> {
        self.inner.deployments()
    }
    fn retain_deployment(&mut self, deployment: &chunk_contract::Deployment) -> chunk_store::Result<()> {
        self.inner.retain_deployment(deployment)
    }

    fn apply_schema(&mut self, schema: &DatabaseSchema) -> chunk_store::Result<Revision> {
        self.inner.apply_schema(schema)
    }
    fn snapshot(&mut self) -> chunk_store::Result<Snapshot> {
        self.inner.snapshot()
    }
    fn outcome(&self, operation: &Operation) -> chunk_store::Result<Option<Outcome>> {
        self.inner.outcome(operation)
    }
    fn epoch(&self) -> chunk_store::Epoch {
        self.inner.epoch()
    }
    fn commit(&mut self, commit: Commit) -> chunk_store::Result<Outcome> {
        if let Some(attempts) = &self.attempts {
            attempts.lock().unwrap().push(commit.result.clone());
        }
        let index = self.committed;
        self.committed += 1;
        let _ = self.notices.send(Notice::Commit(index));
        if let Some(gate) = self.commits.get(index) {
            let _ = gate.recv();
        }
        if self.rejected && index == 0 {
            return Err(chunk_store::Error::Capacity);
        }
        let outcome = self.inner.commit(commit)?;
        if self.ambiguous && index == 0 {
            return Err(chunk_store::Error::Io(std::io::Error::other("lost acknowledgement")));
        }
        Ok(outcome)
    }
}

struct Controls {
    prepare: Option<mpsc::Sender<()>>,
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
        Self::with_failure(ambiguous, false).await
    }

    async fn with_failure(ambiguous: bool, rejected: bool) -> Self {
        Self::with_options(ambiguous, rejected, rejected).await
    }

    async fn with_options(ambiguous: bool, rejected: bool, gate_prepare: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let mut store = open(&directory);
        let base = store.snapshot().unwrap().revision;
        let (commits, receivers): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::channel()).unzip();
        let (notices, receiver) = signals::unbounded_channel();
        let (prepare, prepare_receiver) = if gate_prepare {
            let (sender, receiver) = mpsc::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        let store = ControlledStore {
            prepare: prepare_receiver,
            commits: receivers,
            ambiguous,
            rejected,
            ..ControlledStore::new(store, notices)
        };
        let backend = Backend::new("local".into(), Box::new(store)).unwrap();
        backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
        Self { controls: Controls { prepare, commits, notices: receiver }, backend, directory, base }
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
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    let mut second = Box::pin(backend.mutate("second".into(), call("bump", json!({"id": "p"}))));
    pending(second.as_mut()).await;
    let independent = backend.query(call("get", json!({"id": "unrelated"}))).await.unwrap();
    assert_eq!(independent.revision, harness.base);
    assert_eq!(value(&independent), json!(0));
    let mut query = Box::pin(backend.query(call("get", json!({"id": "p"}))));
    pending(query.as_mut()).await;
    backend.register(DeploymentId::new("other").unwrap(), SOURCE.into(), Limits::default()).await.unwrap();
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
    let query = query.await.unwrap();
    assert_eq!(query.revision, first.revision);
    assert_eq!(value(&query), json!(1));
    let mut query = Box::pin(backend.query(call("get", json!({"id":"p"}))));
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
async fn duplicate_waiters_share_one_mutation_and_cancelled_waiter_does_not_stage_twice() {
    let mut harness = Harness::new(false).await;
    let mut requests = Vec::new();
    for index in 0..64 {
        let mut request = Box::pin(harness.backend.mutate("same".into(), call("bump", json!({"id": "p"}))));
        pending(request.as_mut()).await;
        if index == 0 {
            // Let preparation finish before the duplicates arrive.
            assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
        }
        requests.push(request);
    }
    drop(requests.remove(0));
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
async fn query_replies_waiting_for_a_stalled_commit_hold_request_memory() {
    let mut harness = Harness::new(false).await;
    let large = DeploymentId::new("large").unwrap();
    let source = format!(
        "{SOURCE} export function large(ctx) {{ return ctx.db.get('profiles','p') ? 'x'.repeat(1000000) : ''; }}"
    );
    harness.backend.register(large.clone(), source, Limits::default()).await.unwrap();
    let mut stalled = Box::pin(harness.backend.mutate("stalled".into(), call("bump", json!({"id": "p"}))));
    pending(stalled.as_mut()).await;
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    // Each 1 MB reply reads the staged write, so it waits for the commit; 64 MiB holds about 67.
    let queries: Vec<_> = (0..100)
        .map(|_| {
            let backend = harness.backend.clone();
            let call = Call { deployment: large.clone(), ..call("large", json!({})) };
            tokio::spawn(async move { backend.query(call).await })
        })
        .collect();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while queries.iter().filter(|query| query.is_finished()).count() < 30 {
        assert!(tokio::time::Instant::now() < deadline, "retained replies were not charged");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    harness.controls.commits[0].send(()).unwrap();
    stalled.await.unwrap();
    let mut delivered = 0;
    for query in queries {
        match query.await.unwrap() {
            Ok(_) => delivered += 1,
            Err(error) => assert!(matches!(error, Error::Overloaded(Limit::RequestMemory)), "{error}"),
        }
    }
    assert!((1..=70).contains(&delivered), "{delivered} replies delivered");
}

#[tokio::test]
async fn ambiguous_commit_stops_the_suffix_and_restart_recovers_once() {
    let mut harness = Harness::new(true).await;
    let mut first = Box::pin(harness.backend.mutate("first".into(), call("bump", json!({"id": "p"}))));
    pending(first.as_mut()).await;
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    let mut second = Box::pin(harness.backend.mutate("second".into(), call("bump", json!({"id": "p"}))));
    pending(second.as_mut()).await;
    harness.backend.register(DeploymentId::new("barrier").unwrap(), SOURCE.into(), Limits::default()).await.unwrap();
    harness.controls.commits[0].send(()).unwrap();
    assert!(matches!(first.await, Err(Error::CommitFailed)));
    assert!(matches!(second.await, Err(Error::CommitFailed)));
    assert!(matches!(harness.backend.query(call("get", json!({"id": "p"}))).await, Err(Error::CommitFailed)));
    let Harness { controls, backend, directory, .. } = harness;
    drop(controls);
    drop(backend);
    let mut store = open(&directory);
    let snapshot = store.snapshot().unwrap();
    assert_eq!(snapshot.get(&DocumentKey::new("profiles", "p").unwrap()).unwrap().unwrap().value, json!({"coins": 1}));
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    let recovered = backend.mutate("first".into(), call("bump", json!({"id": "p"}))).await.unwrap();
    assert_eq!(value(&recovered), json!(1));
    let mut changed = call("bump", json!({"id": "p"}));
    changed.caller = json!({"player": "someone else"}).into();
    assert!(
        matches!(backend.mutate("first".into(), changed).await, Err(Error::Storage(error)) if matches!(error.as_ref(), chunk_store::Error::OperationMismatch))
    );
    assert_eq!(value(&backend.mutate("second".into(), call("bump", json!({"id": "p"}))).await.unwrap()), json!(2));
}

#[tokio::test]
async fn subscriptions_track_empty_ranges_and_update_dependencies_when_results_match() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::new("local".into(), Box::new(open(&directory))).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    for (key, document) in
        [("a", json!({"coins": 1})), ("b", json!({"coins": 1})), ("selector", json!({"selected": "a"}))]
    {
        backend.mutate(key.into(), call("put", json!({"id": key, "value": document}))).await.unwrap();
    }
    let mut selected = backend.subscribe(call("selected", json!({}))).await.unwrap();
    assert_eq!(value(&selected.next().await.unwrap()), json!(1));
    backend.mutate("switch".into(), call("put", json!({"id": "selector", "value": {"selected": "b"}}))).await.unwrap();
    backend.mutate("bump-b".into(), call("bump", json!({"id": "b"}))).await.unwrap();
    assert_eq!(value(&selected.next().await.unwrap()), json!(2));
    let mut range = backend.subscribe(call("scan", json!({}))).await.unwrap();
    assert_eq!(value(&range.next().await.unwrap()), json!([]));
    backend.mutate("insert".into(), call("put", json!({"id": "x", "value": {"coins": 3}}))).await.unwrap();
    assert_eq!(value(&range.next().await.unwrap()), json!([["x", {"coins": 3}]]));
    backend.mutate("remove".into(), call("remove", json!({"id": "x"}))).await.unwrap();
    assert_eq!(value(&range.next().await.unwrap()), json!([]));
    assert!(matches!(backend.release(id()).await, Err(Error::Busy)));
    drop(selected);
    drop(range);
    assert!(backend.release(id()).await.unwrap());
}

#[tokio::test]
async fn rejected_commit_drains_suffix_before_reusing_revisions() {
    let mut harness = Harness::with_failure(false, true).await;
    let backend = &harness.backend;
    let mut watch = backend.subscribe(call("get", json!({"id":"p"}))).await.unwrap();
    watch.next().await.unwrap();
    let mut first = Box::pin(backend.mutate("a".into(), call("bump", json!({"id":"p"}))));
    pending(first.as_mut()).await;
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Preparing);
    let mut second = Box::pin(backend.mutate("b".into(), call("bump", json!({"id":"p"}))));
    pending(second.as_mut()).await;
    backend.query(call("get", json!({"id":"other"}))).await.unwrap();
    harness.controls.prepare.take().unwrap().send(()).unwrap();
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    backend.query(call("get", json!({"id":"other"}))).await.unwrap();
    harness.controls.commits[0].send(()).unwrap();
    assert!(matches!(first.await, Err(Error::Retry)));
    assert!(matches!(second.await, Err(Error::Retry)));
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(1));
    assert_eq!(value(&backend.query(call("get", json!({"id":"p"}))).await.unwrap()), json!(0));
    assert!(matches!(backend.mutate("c".into(), call("bump", json!({"id":"p"}))).await, Err(Error::Busy)));
    harness.controls.commits[1].send(()).unwrap();
    let result = loop {
        match backend.mutate("c".into(), call("bump", json!({"id":"p"}))).await {
            Err(Error::Busy) => tokio::task::yield_now().await,
            result => break result.unwrap(),
        }
    };
    assert_eq!(value(&result), json!(1));
    assert_eq!(result.revision, Revision(harness.base.0 + 1));
    assert_eq!(value(&watch.next().await.unwrap()), json!(1));
}

#[tokio::test]
async fn invalid_results_leave_backend_usable_and_watches_recover_from_data_errors() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::new("local".into(), Box::new(open(&directory))).unwrap();
    let source = format!(
        "{SOURCE} export function strict(ctx) {{ return ctx.db.get('profiles','p').coins; }} export function invalid(ctx,args) {{ ctx.db.put('profiles','p',{{coins:99}}); return args.deep ? Array.from({{length:130}}).reduce(v=>[v],null) : '\\ud800'; }}"
    );
    backend.register(id(), source, Limits::default()).await.unwrap();
    for deep in [true, false] {
        assert!(backend.mutate(format!("bad-{deep}"), call("invalid", json!({"deep":deep}))).await.is_err());
        assert_eq!(value(&backend.query(call("get", json!({"id":"p"}))).await.unwrap()), json!(0));
    }
    backend.mutate("put".into(), call("put", json!({"id":"p","value":{"coins":3}}))).await.unwrap();
    let mut watch = backend.subscribe(call("strict", json!({}))).await.unwrap();
    assert_eq!(value(&watch.next().await.unwrap()), json!(3));
    backend.mutate("delete".into(), call("remove", json!({"id":"p"}))).await.unwrap();
    assert!(matches!(watch.next().await, Err(Error::JavaScript(_))));
    backend.mutate("restore".into(), call("put", json!({"id":"p","value":{"coins":3}}))).await.unwrap();
    assert_eq!(value(&watch.next().await.unwrap()), json!(3));
    let next = DeploymentId::new("next").unwrap();
    backend
        .register(next.clone(), format!("{SOURCE} export function unused() {{ return 99; }}"), Limits::default())
        .await
        .unwrap();
    let mut retry = call("put", json!({"id":"p","value":{"coins":3}}));
    retry.deployment = next;
    let outcome = backend.mutate("put".into(), retry).await.unwrap();
    assert!(outcome.revision < backend.query(call("get", json!({"id":"p"}))).await.unwrap().revision);
}

#[tokio::test]
async fn foreground_queries_run_between_subscription_reevaluations() {
    let directory = tempfile::tempdir().unwrap();
    // One read engine, so its counter sees every rerun and the query competes with them.
    let backend = Backend::with_readers("local".into(), Box::new(open(&directory)), 1).unwrap();
    backend.register(id(), format!("{SOURCE} let evaluations=0; export function slow(ctx, args) {{ const value=ctx.db.get('profiles','p'); if(value) {{ let n=args.n; for(let i=0;i<12000000;i++) n += Math.sqrt(i); evaluations++; return n; }} return 0; }} export function count() {{ return evaluations; }}"), Limits::default()).await.unwrap();
    let mut watches = Vec::new();
    for n in 0..16 {
        let mut watch = backend.subscribe(call("slow", json!({ "n": n }))).await.unwrap();
        watch.next().await.unwrap();
        watches.push(watch);
    }
    backend.mutate("start".into(), call("bump", json!({"id":"p"}))).await.unwrap();
    // Each rerun takes tens of milliseconds; the query queues during the first and runs next.
    let count = value(&backend.query(call("count", json!({}))).await.unwrap()).as_u64().unwrap();
    assert!(count <= 2, "foreground query waited for {count} of 16 reruns");
    for watch in &mut watches {
        watch.next().await.unwrap();
    }
    assert_eq!(value(&backend.query(call("count", json!({}))).await.unwrap()), json!(16));
}

#[tokio::test]
async fn indexed_reads_merge_both_overlays_and_invalidate_old_and_new_keys() {
    let directory = tempfile::tempdir().unwrap();
    let (notices, mut notice) = signals::unbounded_channel();
    let (scheduling, scheduling_gate) = mpsc::channel();
    let (commits, gates): (Vec<_>, Vec<_>) = (0..2).map(|_| mpsc::channel()).unzip();
    let store = ControlledStore {
        scheduling: Some(scheduling_gate),
        commits: gates,
        ..ControlledStore::new(open(&directory), notices)
    };
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    let barrier = || backend.query(call("get", json!({"id":"unrelated"})));
    let mut watch = backend.subscribe(call("indexed", json!({}))).await.unwrap();
    assert_eq!(value(&watch.next().await.unwrap()), json!([]));
    // Both prepares share one durable write, so both mutations stage before either commits.
    let mut blocker = Box::pin(backend.acknowledge_wake(0, None));
    pending(blocker.as_mut()).await;
    assert_eq!(notice.recv().await.unwrap(), Notice::Scheduling);
    let mut seed = Box::pin(backend.mutate("seed-index".into(), call("seedIndex", json!({}))));
    pending(seed.as_mut()).await;
    let mut shift = Box::pin(backend.mutate("shift-index".into(), call("shiftIndex", json!({}))));
    pending(shift.as_mut()).await;
    barrier().await.unwrap();
    scheduling.send(()).unwrap();
    assert!(blocker.await.is_err());
    assert_eq!(notice.recv().await.unwrap(), Notice::Commit(0));
    barrier().await.unwrap();
    let mut query = Box::pin(backend.query(call("indexed", json!({}))));
    pending(query.as_mut()).await;
    commits[0].send(()).unwrap();
    assert_eq!(notice.recv().await.unwrap(), Notice::Commit(1));
    pending(query.as_mut()).await;
    pending(shift.as_mut()).await;
    commits[1].send(()).unwrap();
    seed.await.unwrap();
    assert_eq!(value(&shift.await.unwrap()), json!([["c", {"coins":3}]]));
    assert_eq!(value(&query.await.unwrap()), json!([["c", {"coins":3}]]));
    // Subscribers see the seeded state only if the two commits were written separately.
    let mut published = value(&watch.next().await.unwrap());
    if published == json!([["a", {"coins":1}]]) {
        published = value(&watch.next().await.unwrap());
    }
    assert_eq!(published, json!([["c", {"coins":3}]]));
    backend.mutate("leave-range".into(), call("put", json!({"id":"c", "value":{"coins":9}}))).await.unwrap();
    assert_eq!(value(&watch.next().await.unwrap()), json!([]));
    backend.mutate("enter-range".into(), call("put", json!({"id":"z", "value":{"coins":2}}))).await.unwrap();
    assert_eq!(value(&watch.next().await.unwrap()), json!([["z", {"coins":2}]]));
}

#[tokio::test]
async fn rejected_operation_preserves_time_seed_and_deployment_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let attempts = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (notices, _) = signals::unbounded_channel();
    let store = ControlledStore {
        rejected: true,
        attempts: Some(attempts.clone()),
        ..ControlledStore::new(open(&directory), notices)
    };
    let source = "export function attempt(ctx) { ctx.db.put('profiles','p',{coins:1}); return {time:Date.now(), random:Math.random(), id:crypto.randomUUID()}; }";
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.register(id(), source.into(), Limits::default()).await.unwrap();
    assert!(matches!(backend.mutate("stable".into(), call("attempt", Value::Null)).await, Err(Error::Retry)));
    drop(backend);
    let backend = Backend::new("local".into(), Box::new(open(&directory))).unwrap();
    backend.register(id(), source.into(), Limits::default()).await.unwrap();
    let other = DeploymentId::new("different").unwrap();
    backend.register(other.clone(), source.into(), Limits::default()).await.unwrap();
    let mut changed = call("attempt", Value::Null);
    changed.deployment = other;
    assert!(
        matches!(backend.mutate("stable".into(), changed).await, Err(Error::Storage(error)) if matches!(error.as_ref(), chunk_store::Error::OperationMismatch))
    );
    let recovered = backend.mutate("stable".into(), call("attempt", Value::Null)).await.unwrap();
    assert_eq!(value(&recovered), attempts.lock().unwrap()[0]);
    assert_eq!(
        backend.mutate("stable".into(), call("attempt", Value::Null)).await.unwrap().revision,
        recovered.revision
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_stops_both_wait_for_a_pending_commit() {
    let mut harness = Harness::new(false).await;
    let mut first = Box::pin(harness.backend.mutate("first".into(), call("bump", json!({"id": "p"}))));
    pending(first.as_mut()).await;
    assert_eq!(harness.controls.notices.recv().await.unwrap(), Notice::Commit(0));
    let returned = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stoppers: Vec<_> = (0..2)
        .map(|_| {
            let (backend, returned) = (harness.backend.clone(), returned.clone());
            std::thread::spawn(move || {
                backend.stop();
                returned.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
        })
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(returned.load(std::sync::atomic::Ordering::SeqCst), 0, "a stop returned before the pending commit");
    harness.controls.commits[0].send(()).unwrap();
    for stopper in stoppers {
        stopper.join().unwrap();
    }
    assert_eq!(first.await.unwrap().revision, Revision(harness.base.0 + 1));
}

mod actions;
mod batching;
mod context;
mod documents;
mod effects;
mod integration;
mod jobs;
mod limits;
mod subscriptions;
