mod runtime;

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
export function boom(ctx) { throw 'x'.repeat(17 * 1024 * 1024); }
export function big(ctx) { return 'x'.repeat(900 * 1024) + get(ctx); }
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
        ("boom", function(FunctionKind::Query, Schema::Null)),
        ("big", Function { result: Schema::String, ..function(FunctionKind::Query, Schema::Null) }),
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
    task: JoinHandle<io::Result<()>>,
    control: Arc<Control>,
    client: CoreClient<Channel>,
    cli: String,
}

impl Fixture {
    async fn start() -> Self {
        Self::with_host(Arc::new(Host)).await
    }

    async fn with_host(host: Arc<dyn chunk_control::Host>) -> Self {
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
            listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            control: chunk_control::Config { environment: "test".into() },
            host,
            fresh: false,
            services: Some(services(backend.clone(), Some(PLATFORM.into()))),
        };
        let task = tokio::spawn(chunk_control::server::run(config, ready, stop.clone()));
        let started = started.await.unwrap();
        let client = CoreClient::connect(started.connection.endpoint).await.unwrap();
        Self {
            _directory: directory,
            backend,
            stop,
            task,
            control: started.control,
            client,
            cli: started.connection.token,
        }
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
        self.task.await.unwrap().unwrap();
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

/// Reads position-only updates until one reaches `target`'s position, failing at `deadline`.
async fn advance(updates: &mut Streaming<Update>, target: &CallResponse, deadline: tokio::time::Instant) {
    loop {
        let update = tokio::time::timeout_at(deadline, updates.message()).await.expect("an advance in time");
        let update = update.unwrap().expect("an update");
        assert!(update.upserts.is_empty() && update.removed.is_empty() && !update.snapshot);
        if revision(update.position.as_ref()) >= revision(target.position.as_ref()) {
            return;
        }
    }
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
    // Idle streams advance at most once a second; this one catches up to its own credential's writes sooner.
    let prompt = tokio::time::Instant::now() + Duration::from_millis(900);

    let added = fixture.call(&cli, "add", "add", "5").await;
    let mut update = next(&mut updates).await;
    while update.upserts.is_empty() {
        update = next(&mut updates).await;
    }
    assert_eq!(update.upserts, [entry("5")]);
    assert!(revision(update.position.as_ref()) >= revision(added.position.as_ref()));

    let touched = fixture.call(&cli, "touch", "touch", "null").await;
    advance(&mut updates, &touched, prompt).await;

    // Another credential's write waits for the next idle advance.
    let written = fixture.call(PLATFORM, "gateway-touch", "touch", "null").await;
    let quiet = tokio::time::timeout(Duration::from_millis(500), updates.message()).await;
    assert!(quiet.is_err(), "the idle stream advanced promptly");
    advance(&mut updates, &written, tokio::time::Instant::now() + Duration::from_secs(1)).await;
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_errors_stay_in_band() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let bounded = |error: &Error| {
        assert_eq!(error.code(), Code::Application);
        assert!(error.message.len() < 65 * 1024 && error.message.ends_with("(truncated)"));
    };
    match &fixture.call(&cli, "", "boom", "null").await.outcome {
        Some(Outcome::Error(error)) => bounded(error),
        outcome => panic!("expected an error, got {outcome:?}"),
    }

    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"boom": {"function": "boom"}, "count": {"function": "get"}}"#.to_vec(),
        deployment: "test".into(),
        ..SubscribeRequest::default()
    };
    let mut updates = fixture.client.subscribe(authorized(subscription, &cli)).await.unwrap().into_inner();
    let snapshot = next(&mut updates).await;
    match &snapshot.upserts[0] {
        Entry { key, state: Some(State::Error(error)) } if key == "boom" => bounded(error),
        entry => panic!("expected an error entry, got {:?}", entry.key),
    }
    fixture.call(&cli, "add", "add", "5").await;
    let mut update = next(&mut updates).await;
    while update.upserts.is_empty() {
        update = next(&mut updates).await;
    }
    assert_eq!(update.upserts, [Entry { key: "count".into(), state: Some(State::Value("5".into())) }]);
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stream_whose_client_stopped_reading_releases_its_subscription_and_ends_with_its_error() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"big": {"function": "big"}}"#.to_vec(),
        deployment: "test".into(),
        ..SubscribeRequest::default()
    };
    let mut updates = fixture.client.subscribe(authorized(subscription, &cli)).await.unwrap().into_inner();
    // Each value is most of a mebibyte, and each has time to be sent, so these fill every buffer up to the client.
    for count in 0..16 {
        fixture.call(&cli, &format!("add-{count}"), "add", "1").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A position-only advance while the client is full.
    fixture.call(&cli, "touch", "touch", "null").await;

    fixture.stop.cancel();
    let deployment = chunk_js::DeploymentId::new("test").unwrap();
    let released = async {
        while fixture.backend.release(deployment.clone()).await.is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(2), released).await.expect("the subscription released promptly");
    let mut last = next(&mut updates).await;
    while let Some(update) = tokio::time::timeout(Duration::from_secs(10), updates.message()).await.unwrap().unwrap() {
        last = update;
    }
    assert_eq!(last.error.map(|error| error.code()), Some(Code::Unavailable));
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_stream_ends_once_the_player_moves_to_another_session() {
    use chunk_proto::v1::{ActivateClaim, MovePlayerRequest};
    let (jvm, server) = runtime::Runtime::start();
    let mut fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let control = fixture.control.clone();
    control.activate_release(runtime::release()).unwrap();
    let assignment = control.claim(runtime::login()).await.unwrap();
    let session = assignment.delivery.and_then(|delivery| delivery.session).unwrap().id;
    let host = jvm.host().unwrap();
    // The JVM reports the arrival on its own stream.
    while control.session_scope(&host, &session, Some(runtime::PLAYER)).is_err() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"count": {"function": "get"}}"#.to_vec(),
        deployment: "test".into(),
        caller: Some(Caller { session, player: runtime::PLAYER.into() }),
        ..SubscribeRequest::default()
    };
    let mut updates = fixture.client.subscribe(authorized(subscription, JVM)).await.unwrap().into_inner();
    assert!(next(&mut updates).await.snapshot);

    let request = MovePlayerRequest {
        operation_id: "move".into(),
        player_id: runtime::PLAYER.into(),
        demand: Some(runtime::demand("arena")),
        ..MovePlayerRequest::default()
    };
    let moved = control.claim(control.move_player(request).unwrap()).await.unwrap();
    // Control's change feed ends the stream at once, not at its next idle advance.
    let prompt = tokio::time::Instant::now() + Duration::from_millis(500);
    control.cancel(runtime::login()).await.unwrap();
    control.activate(ActivateClaim { claim: moved.claim }).await.unwrap();
    let update = tokio::time::timeout_at(prompt, updates.message()).await.expect("a prompt end");
    let update = update.unwrap().expect("an update");
    assert_eq!(update.error.map(|error| error.code()), Some(Code::Denied));
    assert!(updates.message().await.unwrap().is_none());
    drop(updates);
    fixture.stop().await;
    server.abort();
}
