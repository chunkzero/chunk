use chunk_contract::{Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_proto::v1::{BackendMutation, BackendQuery, BackendWatchGroup, backend_client::BackendClient};
use chunk_store::{SqliteStore, Storage};
use serde_json::json;
use tokio::{net::TcpListener, sync::oneshot};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{
    Code, Request,
    transport::{Channel, Server},
};

use crate::{Backend, Service};

const CREDENTIAL: &str = "test-credential-with-at-least-32-bytes";
const SOURCE: &str = r"
export function get(ctx) { return ctx.db.get('counters', 'count')?.value ?? 0; }
export function strict(ctx) { return ctx.db.get('counters', 'count').value; }
export function increment(ctx) { const value=get(ctx)+1; ctx.db.put('counters','count',{value}); return value; }
export function badResult(ctx) { ctx.db.put('counters','count',{value:999}); return 'invalid'; }
export function privateRead() { return 123; }
";

fn deployment(id: &str) -> Deployment {
    Deployment {
        domains: None,
        session_methods: None,
        session_configurations: None,
        destinations: None,
        contract_version: 2,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source: SOURCE.into(),
        tables: serde_json::from_value(json!({"counters": {"fields": {"value": {"schema": {"type": "integer"}}}}}))
            .unwrap(),
        functions: [
            ("get", FunctionKind::Query),
            ("strict", FunctionKind::Query),
            ("increment", FunctionKind::Mutation),
            ("badResult", FunctionKind::Mutation),
            ("privateRead", FunctionKind::Query),
        ]
        .into_iter()
        .map(|(name, kind)| {
            (
                name.into(),
                Function {
                    kind,
                    visibility: if name == "privateRead" { Visibility::Internal } else { Visibility::Public },
                    export: name.into(),
                    arguments: Schema::Null,
                    result: Schema::Integer,
                },
            )
        })
        .collect(),
    }
}

fn query(function: &str) -> BackendQuery {
    BackendQuery {
        function: function.into(),
        arguments_json: b"null".to_vec(),
        caller_json: br#"{"service":"test"}"#.to_vec(),
    }
}

fn mutation(function: &str, operation: &str) -> BackendMutation {
    let query = query(function);
    BackendMutation {
        function: query.function,
        arguments_json: query.arguments_json,
        caller_json: query.caller_json,
        operation_id: operation.into(),
    }
}

fn authorized<T>(value: T) -> Request<T> {
    let mut request = Request::new(value);
    request.metadata_mut().insert("authorization", format!("Bearer {CREDENTIAL}").parse().unwrap());
    request.metadata_mut().insert("x-chunk-environment", "local".parse().unwrap());
    request.metadata_mut().insert("x-chunk-deployment", "a".parse().unwrap());
    request
}

struct Running {
    client: BackendClient<Channel>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn start(backend: Backend) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            Server::builder()
                .add_service(Service::new(backend, CREDENTIAL).unwrap().into_server())
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let client = BackendClient::connect(format!("http://{address}")).await.unwrap();
        Self { client, stop: Some(stop), task }
    }

    async fn shutdown(mut self) {
        self.stop.take().unwrap().send(()).unwrap();
        (&mut self.task).await.unwrap();
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn grpc_contracts_groups_and_restart_preserve_one_durable_operation() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backend.db");
    let mut store = SqliteStore::open(&path, "local").unwrap();
    store.apply_schema(&deployment("a").tables).unwrap();
    let backend = Backend::new("local".into(), Box::new(store)).unwrap();
    backend.deploy(deployment("a")).await.unwrap();
    backend.deploy(deployment("b")).await.unwrap();
    let mut server = Running::start(backend.clone()).await;
    verify_contracts(&mut server.client).await;
    let queries = vec![query("get"), query("strict")];
    let mut watch = server
        .client
        .watch_group(authorized(BackendWatchGroup { queries: queries.clone() }))
        .await
        .unwrap()
        .into_inner();
    let initial = watch.message().await.unwrap().unwrap();
    assert_eq!(initial.results_json[0], b"0");
    assert!(initial.results_json[1].is_empty());
    assert!(!initial.errors[1].is_empty());
    // Discard the successful reply: the caller will recover it by identity after restart.
    let reply = server.client.mutate(authorized(mutation("increment", "lost-reply"))).await.unwrap().into_inner();
    let revision = reply.revision;
    drop(reply);
    let update = watch.message().await.unwrap().unwrap();
    assert_eq!(update.revision, revision);
    assert_eq!(update.results_json, vec![b"1".to_vec(), b"1".to_vec()]);
    assert!(update.errors.iter().all(String::is_empty));
    drop(watch);
    server.shutdown().await;
    drop(backend);
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    // Retained bundles/contracts reload without registration or application input.
    let mut changed = deployment("a");
    changed.source.push_str("\n// changed");
    assert!(backend.deploy(changed).await.is_err());
    let mut server = Running::start(backend).await;
    let recovered = server.client.mutate(authorized(mutation("increment", "lost-reply"))).await.unwrap().into_inner();
    assert_eq!(recovered.revision, revision);
    assert_eq!(recovered.result_json, b"1");
    let mut mismatch = mutation("increment", "lost-reply");
    mismatch.caller_json = b"{}".to_vec();
    assert_eq!(server.client.mutate(authorized(mismatch)).await.unwrap_err().code(), Code::AlreadyExists);
    let mut watch = server.client.watch_group(authorized(BackendWatchGroup { queries })).await.unwrap().into_inner();
    let fresh = watch.message().await.unwrap().unwrap();
    assert_eq!(fresh.revision, revision);
    assert_eq!(fresh.results_json, vec![b"1".to_vec(), b"1".to_vec()]);
    drop(watch);
    server.shutdown().await;
}

async fn verify_contracts(client: &mut BackendClient<Channel>) {
    assert_eq!(client.query(query("get")).await.unwrap_err().code(), Code::Unauthenticated);
    for credential in [
        format!("Bearer {CREDENTIAL}extra"),
        format!("Bearer {}", &CREDENTIAL[1..]),
        format!("Bearer X{}", &CREDENTIAL[1..]),
        format!("Bearer {}X", &CREDENTIAL[..CREDENTIAL.len() - 1]),
    ] {
        let mut request = Request::new(query("get"));
        request.metadata_mut().insert("authorization", credential.parse().unwrap());
        assert_eq!(client.query(request).await.unwrap_err().code(), Code::Unauthenticated);
    }
    let mut wrong_environment = authorized(query("get"));
    wrong_environment.metadata_mut().insert("x-chunk-environment", "other".parse().unwrap());
    assert_eq!(client.query(wrong_environment).await.unwrap_err().code(), Code::PermissionDenied);
    assert_eq!(client.query(authorized(query("privateRead"))).await.unwrap_err().code(), Code::NotFound);
    assert_eq!(
        client.mutate(authorized(mutation("badResult", "invalid"))).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    client.check_deployment(authorized(())).await.unwrap();
    let mut missing = authorized(());
    missing.metadata_mut().insert("x-chunk-deployment", "missing".parse().unwrap());
    assert_eq!(client.check_deployment(missing).await.unwrap_err().code(), Code::NotFound);
    for binding in ["x-chunk-environment", "x-chunk-deployment"] {
        let mut unbound = authorized(query("get"));
        unbound.metadata_mut().remove(binding);
        assert_eq!(client.query(unbound).await.unwrap_err().code(), Code::InvalidArgument);
    }
    assert_eq!(client.mutate(authorized(mutation("increment", ""))).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(client.query(authorized(query("increment"))).await.unwrap_err().code(), Code::InvalidArgument);
    assert_eq!(
        client.mutate(authorized(mutation("get", "wrong-kind"))).await.unwrap_err().code(),
        Code::InvalidArgument
    );
    assert_eq!(client.query(authorized(query("get"))).await.unwrap().into_inner().result_json, b"0");
}

#[tokio::test]
async fn activation_installs_schema_and_release_is_durable_after_references_drain() {
    use crate::Call;
    use chunk_js::DeploymentId;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("activation.db");
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    backend.deploy(deployment("old")).await.unwrap();
    let call = |version: &str, function: &str| Call {
        deployment: DeploymentId::new(version).unwrap(),
        function: function.into(),
        arguments: serde_json::Value::Null.into(),
        caller: serde_json::Value::Null.into(),
    };
    let first = backend.mutate("first".into(), call("old", "increment")).await.unwrap();
    let mut next = deployment("new");
    next.tables
        .get_mut("counters")
        .unwrap()
        .fields
        .insert("label".into(), serde_json::from_value(json!({"schema":{"type":"string"},"optional":true})).unwrap());
    next.tables.get_mut("counters").unwrap().indexes.insert("by_value".into(), vec!["value".into()]);
    backend.deploy(next.clone()).await.unwrap();
    let mut group = backend.subscribe_group(vec![call("old", "get"), call("new", "get")]).await.unwrap();
    let initial = group.next().await.unwrap();
    assert!(initial.revision > first.revision);
    assert!(matches!(backend.release(DeploymentId::new("old").unwrap()).await, Err(crate::Error::Busy)));
    let second = backend.mutate("second".into(), call("new", "increment")).await.unwrap();
    let update = group.next().await.unwrap();
    assert_eq!(update.revision, second.revision);
    assert_eq!(&*update.results[0].as_ref().unwrap().clone(), "2");
    assert_eq!(&*update.results[1].as_ref().unwrap().clone(), "2");
    let mut bad = next.clone();
    bad.id = "bad".into();
    bad.tables.get_mut("counters").unwrap().fields.get_mut("value").unwrap().schema = Schema::String;
    assert!(matches!(backend.deploy(bad).await, Err(crate::Error::Contract)));
    assert!(backend.query(call("bad", "get")).await.is_err());
    assert_eq!(&*backend.query(call("old", "get")).await.unwrap().json, "2");
    assert!(matches!(backend.mutate("failed-old".into(), call("old", "badResult")).await, Err(crate::Error::Contract)));
    drop(group);
    assert!(backend.release(DeploymentId::new("old").unwrap()).await.unwrap());
    assert!(matches!(backend.deploy(deployment("old")).await, Err(crate::Error::Contract)));
    assert_eq!(&*backend.query(call("new", "get")).await.unwrap().json, "2");
    drop(backend);
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    assert!(backend.query(call("old", "get")).await.is_err());
    assert!(backend.query(call("bad", "get")).await.is_err());
    assert_eq!(&*backend.query(call("new", "get")).await.unwrap().json, "2");
    let mut changed = deployment("old");
    changed.source.push_str("\n// changed");
    assert!(matches!(backend.deploy(changed).await, Err(crate::Error::Contract)));
    assert_eq!(backend.mutate("first".into(), call("new", "increment")).await.unwrap().revision, first.revision);
    assert!(matches!(backend.mutate("failed-old".into(), call("new", "badResult")).await,
            Err(crate::Error::Storage(error)) if matches!(error.as_ref(), chunk_store::Error::OperationMismatch)));
    assert!(backend.mutate("bad-result".into(), call("new", "badResult")).await.is_err());
    assert_eq!(&*backend.query(call("new", "get")).await.unwrap().json, "2");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_shutdown_closes_watchers_and_releases_durable_state() {
    use std::time::Duration;
    use tokio_util::sync::CancellationToken;
    let directory = tempfile::tempdir().unwrap();
    let bundle = directory.path().join("bundle.json");
    std::fs::write(&bundle, serde_json::to_vec(&deployment("a")).unwrap()).unwrap();
    let path = directory.path().join("connection.json");
    // Reopening the same database proves the service joined its worker owners.
    for _ in 0..2 {
        let stop = CancellationToken::new();
        let (ready, started) = oneshot::channel();
        let task = tokio::spawn(crate::server::run(
            crate::server::Config {
                bundle: bundle.clone(),
                environment: "local".into(),
                state: directory.path().join("state"),
                connection: path.clone(),
                bind: "127.0.0.1:0".parse().unwrap(),
            },
            ready,
            stop.clone(),
        ));
        let connection = tokio::time::timeout(Duration::from_secs(10), started).await.unwrap().unwrap();
        let mut client = BackendClient::connect(connection.endpoint).await.unwrap();
        let mut request = authorized(BackendWatchGroup { queries: vec![query("get")] });
        request.metadata_mut().insert("authorization", format!("Bearer {}", connection.token).parse().unwrap());
        let mut stream = client.watch_group(request).await.unwrap().into_inner();
        assert!(stream.message().await.unwrap().is_some());
        stop.cancel();
        tokio::time::timeout(Duration::from_secs(10), task).await.unwrap().unwrap().unwrap();
        assert!(!path.exists());
        assert!(stream.message().await.unwrap().is_none());
    }
}

#[tokio::test]
async fn optional_null_arguments_recover_the_same_mutation_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("normalized.db");
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    let mut version = deployment("a");
    version.functions.get_mut("increment").unwrap().arguments = Schema::Object {
        fields: [("note".into(), chunk_contract::Field { schema: Schema::String, optional: true })].into(),
    };
    version.source.push_str("\nexport function optional(ctx, args) { if (Object.hasOwn(args, 'note')) throw Error('expected omission'); return increment(ctx); }");
    version.functions.get_mut("increment").unwrap().export = "optional".into();
    backend.deploy(version).await.unwrap();
    let mut server = Running::start(backend.clone()).await;
    let mut first = mutation("increment", "normalized-retry");
    first.arguments_json = br#"{"note":null}"#.to_vec();
    let result = server.client.mutate(authorized(first)).await.unwrap().into_inner();
    server.shutdown().await;
    drop(backend);
    let backend = Backend::new("local".into(), Box::new(SqliteStore::open(&path, "local").unwrap())).unwrap();
    let mut server = Running::start(backend).await;
    let mut retry = mutation("increment", "normalized-retry");
    retry.arguments_json = b"{}".to_vec();
    assert_eq!(server.client.mutate(authorized(retry.clone())).await.unwrap().into_inner(), result);
    retry.arguments_json = br#"{"note":"changed"}"#.to_vec();
    assert_eq!(server.client.mutate(authorized(retry)).await.unwrap_err().code(), Code::AlreadyExists);
    assert_eq!(server.client.query(authorized(query("get"))).await.unwrap().into_inner().result_json, b"1");
    server.shutdown().await;
}
