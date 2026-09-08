use chunk_contract::{Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_proto::v1::{BackendCall, BackendWatch, backend_client::BackendClient};
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
        contract_version: 1,
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
                    visibility: if name == "privateRead" {
                        Visibility::Internal
                    } else {
                        Visibility::Public
                    },
                    export: name.into(),
                    arguments: Schema::Null,
                    result: Schema::Integer,
                },
            )
        })
        .collect(),
    }
}

fn request(function: &str, operation: &str) -> BackendCall {
    BackendCall {
        environment: "local".into(),
        deployment: "a".into(),
        function: function.into(),
        arguments_json: b"null".to_vec(),
        caller_json: br#"{"service":"test"}"#.to_vec(),
        operation_id: operation.into(),
    }
}

fn authorized<T>(value: T) -> Request<T> {
    let mut request = Request::new(value);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {CREDENTIAL}").parse().unwrap());
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
        Self {
            client,
            stop: Some(stop),
            task,
        }
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
    let queries = vec![
        request("get", ""),
        BackendCall {
            deployment: "b".into(),
            ..request("strict", "")
        },
    ];
    let mut watch = server
        .client
        .watch(authorized(BackendWatch {
            queries: queries.clone(),
        }))
        .await
        .unwrap()
        .into_inner();
    let initial = watch.message().await.unwrap().unwrap();
    assert_eq!(initial.results_json[0], b"0");
    assert!(initial.results_json[1].is_empty());
    assert!(!initial.errors[1].is_empty());
    // Discard the successful reply: the caller will recover it by identity after restart.
    let reply = server
        .client
        .call(authorized(request("increment", "lost-reply")))
        .await
        .unwrap()
        .into_inner();
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
    let recovered = server
        .client
        .call(authorized(BackendCall {
            deployment: "b".into(),
            ..request("increment", "lost-reply")
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(recovered.revision, revision);
    assert_eq!(recovered.result_json, b"1");
    let mut mismatch = request("increment", "lost-reply");
    mismatch.caller_json = b"{}".to_vec();
    assert_eq!(
        server.client.call(authorized(mismatch)).await.unwrap_err().code(),
        Code::AlreadyExists
    );
    let mut watch = server
        .client
        .watch(authorized(BackendWatch { queries }))
        .await
        .unwrap()
        .into_inner();
    let fresh = watch.message().await.unwrap().unwrap();
    assert_eq!(fresh.revision, revision);
    assert_eq!(fresh.results_json, vec![b"1".to_vec(), b"1".to_vec()]);
    drop(watch);
    server.shutdown().await;
}

async fn verify_contracts(client: &mut BackendClient<Channel>) {
    assert_eq!(
        client.call(request("get", "")).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    for credential in [
        format!("Bearer {CREDENTIAL}extra"),
        format!("Bearer {}", &CREDENTIAL[1..]),
        format!("Bearer X{}", &CREDENTIAL[1..]),
        format!("Bearer {}X", &CREDENTIAL[..CREDENTIAL.len() - 1]),
    ] {
        let mut request = Request::new(request("get", ""));
        request
            .metadata_mut()
            .insert("authorization", credential.parse().unwrap());
        assert_eq!(client.call(request).await.unwrap_err().code(), Code::Unauthenticated);
    }
    let mut wrong_environment = request("get", "");
    wrong_environment.environment = "other".into();
    assert_eq!(
        client.call(authorized(wrong_environment)).await.unwrap_err().code(),
        Code::PermissionDenied
    );
    assert_eq!(
        client
            .call(authorized(request("privateRead", "")))
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client
            .call(authorized(request("badResult", "invalid")))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(
        client
            .call(authorized(request("get", "")))
            .await
            .unwrap()
            .into_inner()
            .result_json,
        b"0"
    );
}
