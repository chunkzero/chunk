use super::*;
use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_proto::sync::v1::{Entry, call_response::Outcome, core_client::CoreClient, entry::State, error::Code};
use std::{io, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tonic::{Streaming, transport::Channel};

const PLATFORM: &str = "platform-credential-with-at-least-32-bytes";
const JVM: &str = "jvm-credential-with-at-least-32-bytes-long";
const SOURCE: &str = r"
export function get(ctx) { return ctx.db.get('counters', 'count')?.value ?? 0; }
export function add(ctx, by) { const value = get(ctx) + by; ctx.db.put('counters', 'count', {value}); return value; }
export function touch(ctx) { ctx.db.put('counters', 'other', {value: 1}); return 1; }
";

fn deployment() -> Deployment {
    let function = |kind, arguments| Function {
        kind,
        visibility: Visibility::Public,
        export: String::new(),
        arguments,
        result: Schema::Integer,
    };
    let functions = [
        ("get", function(FunctionKind::Query, Schema::Null)),
        ("add", function(FunctionKind::Mutation, Schema::Integer)),
        ("touch", function(FunctionKind::Mutation, Schema::Null)),
    ];
    Deployment {
        contracts: Contracts::default(),
        contract_version: 2,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: "test".into(),
        source: SOURCE.into(),
        tables: serde_json::from_value(
            serde_json::json!({"counters": {"fields": {"value": {"schema": {"type": "integer"}}}}}),
        )
        .unwrap(),
        functions: functions
            .into_iter()
            .map(|(name, mut function)| {
                function.export = name.into();
                (name.into(), function)
            })
            .collect(),
    }
}

/// Runs no JVMs; its one process credential belongs to `host-1`.
struct Host;

#[tonic::async_trait]
impl chunk_control::Host for Host {
    async fn ensure(
        &self,
        _id: &str,
        _release: &chunk_control::Release,
        _app: &str,
        _profile: &str,
    ) -> chunk_control::Result<chunk_control::Progress> {
        Ok(chunk_control::Progress::Pending)
    }
    async fn release(&self, _id: &str) -> chunk_control::Result<bool> {
        Ok(true)
    }
    fn stopped(&self, _id: &str) -> bool {
        true
    }
    fn authenticate(&self, credential: &str) -> Option<String> {
        (credential == JVM).then(|| "host-1".into())
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    backend: Backend,
    stop: CancellationToken,
    control: JoinHandle<io::Result<()>>,
    client: CoreClient<Channel>,
    cli: String,
}

impl Fixture {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = chunk_store::SqliteStore::open(directory.path().join("environment.sqlite"), "test").unwrap();
        let backend = Backend::new("test".into(), Box::new(store)).unwrap();
        backend.deploy(deployment()).await.unwrap();
        let (ready, started) = oneshot::channel();
        let stop = CancellationToken::new();
        let config = chunk_control::server::Config {
            state: directory.path().join("control"),
            system: backend.system(),
            connection: directory.path().join("control.json"),
            bind: "127.0.0.1:0".parse().unwrap(),
            control: chunk_control::Config { environment: "test".into() },
            host: Arc::new(Host),
            fresh: false,
            services: Some(services(backend.clone(), Some(PLATFORM.into()))),
        };
        let control = tokio::spawn(chunk_control::server::run(config, ready, stop.clone()));
        let connection = started.await.unwrap().connection;
        let client = CoreClient::connect(connection.endpoint).await.unwrap();
        Self { _directory: directory, backend, stop, control, client, cli: connection.token }
    }

    async fn call(&mut self, credential: &str, operation: &str, method: &str, arguments: &str) -> CallResponse {
        self.call_as(credential, operation, method, arguments, None).await
    }

    async fn call_as(
        &mut self,
        credential: &str,
        operation: &str,
        method: &str,
        arguments: &str,
        caller: Option<Caller>,
    ) -> CallResponse {
        let message = CallRequest {
            operation_id: operation.into(),
            method: method.into(),
            arguments: arguments.into(),
            deployment: "test".into(),
            caller,
            stream: String::new(),
        };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn stop(self) {
        self.stop.cancel();
        self.control.await.unwrap().unwrap();
        let backend = self.backend;
        tokio::task::spawn_blocking(move || drop(backend)).await.unwrap();
    }
}

fn authorized<T>(message: T, credential: &str) -> Request<T> {
    let mut request = Request::new(message);
    if !credential.is_empty() {
        request.metadata_mut().insert("authorization", format!("Bearer {credential}").parse().unwrap());
    }
    request
}

fn code(response: &CallResponse) -> Code {
    match &response.outcome {
        Some(Outcome::Error(error)) => error.code(),
        outcome => panic!("expected an error, got {outcome:?}"),
    }
}

fn revision(position: Option<&Position>) -> u64 {
    position.expect("a position").revision
}

async fn next(updates: &mut Streaming<Update>) -> Update {
    let update = tokio::time::timeout(Duration::from_secs(10), updates.message()).await.unwrap().unwrap();
    update.expect("an update")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retried_mutations_return_their_committed_outcome() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let first = fixture.call(&cli, "add-once", "add", "2").await;
    assert_eq!(first.outcome, Some(Outcome::Result(b"2".to_vec())));
    assert!(first.position.is_some());
    assert_eq!(fixture.call(&cli, "add-once", "add", "2").await, first);
    assert_eq!(code(&fixture.call(&cli, "add-once", "add", "3").await), Code::OperationMismatch);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_credentials_and_foreign_sessions_are_refused() {
    let mut fixture = Fixture::start().await;
    for credential in ["", "not-a-credential"] {
        let message = CallRequest { method: "get".into(), deployment: "test".into(), ..CallRequest::default() };
        let status = fixture.client.call(authorized(message, credential)).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::Unauthenticated);
    }
    let caller = Caller { session: "elsewhere".into(), player: String::new() };
    assert_eq!(code(&fixture.call_as(JVM, "", "get", "null", Some(caller)).await), Code::Denied);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_streams_follow_mutations_and_advance_past_unrelated_ones() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"count": {"function": "get"}}"#.to_vec(),
        deployment: "test".into(),
        ..SubscribeRequest::default()
    };
    let mut updates = fixture.client.subscribe(authorized(subscription, &cli)).await.unwrap().into_inner();
    let entry = |value: &str| Entry { key: "count".into(), state: Some(State::Value(value.into())) };
    let snapshot = next(&mut updates).await;
    assert!(snapshot.snapshot && !snapshot.stream.is_empty());
    assert_eq!(snapshot.upserts, [entry("0")]);

    let added = fixture.call(&cli, "add", "add", "5").await;
    let mut update = next(&mut updates).await;
    while update.upserts.is_empty() {
        update = next(&mut updates).await;
    }
    assert_eq!(update.upserts, [entry("5")]);
    assert!(revision(update.position.as_ref()) >= revision(added.position.as_ref()));

    let touched = fixture.call(&cli, "touch", "touch", "null").await;
    loop {
        let update = next(&mut updates).await;
        assert!(update.upserts.is_empty() && update.removed.is_empty() && !update.snapshot);
        if revision(update.position.as_ref()) >= revision(touched.position.as_ref()) {
            break;
        }
    }
    drop(updates);
    fixture.stop().await;
}
