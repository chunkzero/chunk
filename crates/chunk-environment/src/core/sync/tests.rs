mod claims;
mod effects;
mod hooks;
mod jvm;
mod jvm_effects;
mod runtime;

use super::*;
use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_proto::sync::v1::{
    Cursor, Entry, call_response::Outcome, core_client::CoreClient, entry::State, error::Code,
};
use std::{io, time::Duration};
use tokio::{sync::oneshot, task::JoinHandle};
use tonic::{Streaming, transport::Channel};

const JVM: &str = "jvm-credential-with-at-least-32-bytes-long";
const SOURCE: &str = r"
export function get(ctx) { return ctx.db.get('counters', 'count')?.value ?? 0; }
export function add(ctx, by) { const value = get(ctx) + by; ctx.db.put('counters', 'count', {value}); return value; }
export function touch(ctx) { ctx.db.put('counters', 'other', {value: 1}); return 1; }
export function boom(ctx) { throw 'x'.repeat(17 * 1024 * 1024); }
export function big(ctx) { return 'x'.repeat(900 * 1024) + get(ctx); }
export async function bump(ctx, by) { return await ctx.runMutation('add', by); }
export async function slow(ctx, by) { await ctx.runMutation('add', by); await ctx.sleep(1000); return await ctx.runMutation('add', by); }
export function fill(ctx, text) { return text; }
export function login(ctx) { return {allow: true, reason: JSON.stringify(ctx.caller)}; }
";
const LOGIN: &str = "shared/domains/hooks/login";

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
        ("bump", function(FunctionKind::Action, Schema::Integer)),
        ("slow", function(FunctionKind::Action, Schema::Integer)),
        ("fill", Function { result: Schema::String, ..function(FunctionKind::Action, Schema::String) }),
    ];
    let domains = serde_json::json!({
        "version": 1, "scopes": {"": {"parent": null}}, "apps": {},
        "hooks": {LOGIN: {"domain": "", "event": "player.login", "export": "login"}}
    });
    Deployment {
        contracts: Contracts { domains: Some(serde_json::from_value(domains).unwrap()), ..Contracts::default() },
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
    directory: tempfile::TempDir,
    backend: Backend,
    stop: CancellationToken,
    task: JoinHandle<io::Result<()>>,
    control: Arc<Control>,
    /// Core's endpoint.
    endpoint: String,
    client: CoreClient<Channel>,
    cli: String,
    /// The credential of gateway `proxy`, which holds the fake JVM's claims.
    gateway: String,
    gateways: Arc<Gateways>,
}

impl Fixture {
    async fn start() -> Self {
        Self::with_host(Arc::new(Host)).await
    }

    async fn with_host(host: Arc<dyn chunk_control::Host>) -> Self {
        Self::open(tempfile::tempdir().unwrap(), host).await
    }

    /// Starts core over the store in `directory`, which a stopped core may have left.
    async fn open(directory: tempfile::TempDir, host: Arc<dyn chunk_control::Host>) -> Self {
        let store = chunk_store::SqliteStore::open(directory.path().join("environment.sqlite"), "test").unwrap();
        let backend = Backend::new("test".into(), Box::new(store)).unwrap();
        backend.deploy(deployment()).await.unwrap();
        let (ready, started) = oneshot::channel();
        let stop = CancellationToken::new();
        let gateways = Arc::new(Gateways::default());
        let gateway = gateways.mint("proxy");
        let config = chunk_control::server::Config {
            state: directory.path().join("control"),
            system: backend.system(),
            connection: directory.path().join("control.json"),
            listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
            control: chunk_control::Config { environment: "test".into() },
            host,
            fresh: false,
            services: Some(services(backend.clone(), gateways.clone())),
        };
        let task = tokio::spawn(chunk_control::server::run(config, ready, stop.clone()));
        let started = started.await.unwrap();
        let endpoint = started.connection.endpoint;
        let client = CoreClient::connect(endpoint.clone()).await.unwrap();
        Self {
            directory,
            backend,
            stop,
            task,
            control: started.control,
            endpoint,
            client,
            cli: started.connection.token,
            gateway,
            gateways,
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

    /// Reads the count as `credential`, naming `stream`.
    async fn call_on(&mut self, credential: &str, stream: &str) -> CallResponse {
        let message = CallRequest {
            method: "get".into(),
            arguments: "null".into(),
            deployment: "test".into(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn stop(self) {
        self.close().await;
    }

    /// Stops core and starts it again over the same store with `host`, as after a restart.
    async fn restart(self, host: Arc<dyn chunk_control::Host>) -> Self {
        let control = self.control.clone();
        let directory = self.close().await;
        // Streams still ending hold control, and with it the store the restart reopens.
        let ended = async {
            while Arc::strong_count(&control) > 1 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), ended).await.expect("core's streams ended");
        tokio::task::spawn_blocking(move || drop(control)).await.unwrap();
        Self::open(directory, host).await
    }

    /// Stops core, returning the directory of its store.
    async fn close(self) -> tempfile::TempDir {
        self.stop.cancel();
        self.task.await.unwrap().unwrap();
        let backend = self.backend;
        tokio::task::spawn_blocking(move || drop(backend)).await.unwrap();
        self.directory
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
    let gateway = fixture.gateway.clone();
    let written = fixture.call(&gateway, "gateway-touch", "touch", "null").await;
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

#[tokio::test]
async fn core_stops_within_its_grace_while_a_client_never_reads() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"big": {"function": "big"}}"#.to_vec(),
        deployment: "test".into(),
        ..SubscribeRequest::default()
    };
    let mut updates = fixture.client.subscribe(authorized(subscription, &cli)).await.unwrap().into_inner();
    for count in 0..16 {
        fixture.call(&cli, &format!("add-{count}"), "add", "1").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    tokio::time::pause();
    let stopping = tokio::time::Instant::now();
    fixture.stop().await;
    assert!(stopping.elapsed() < chunk_service::GRACE + Duration::from_secs(1));
    tokio::time::resume();
    // The connection was closed rather than drained, so the stream breaks off without its final update.
    let ended = loop {
        match updates.message().await {
            Ok(Some(_)) => {}
            ended => break ended,
        }
    };
    assert!(ended.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_player_stream_ends_once_the_player_moves_to_another_session() {
    use chunk_proto::v1::{ActivateClaim, ClaimPhase, MovePlayerRequest, PlayerList};
    let (jvm, server) = runtime::Runtime::start();
    let mut fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let control = fixture.control.clone();
    control.activate_release(runtime::release()).unwrap();
    let assignment = control.claim(runtime::login()).await.unwrap();
    let session = assignment.delivery.and_then(|delivery| delivery.session).unwrap().id;
    // The JVM reports the arrival on its own stream.
    let arrived = |list: PlayerList| list.players.iter().any(|player| player.phase() == ClaimPhase::Arrived);
    while !arrived(control.players().unwrap()) {
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newer_gateway_stream_resumes_and_supersedes_the_older_one() {
    let mut fixture = Fixture::start().await;
    let gateway = fixture.gateway.clone();
    let subscription = SubscribeRequest { topic: "gateway/proxy".into(), ..SubscribeRequest::default() };
    let foreign = SubscribeRequest { topic: "gateway/other".into(), ..SubscribeRequest::default() };
    let mut denied = fixture.client.subscribe(authorized(foreign, &gateway)).await.unwrap().into_inner();
    assert_eq!(next(&mut denied).await.error.map(|error| error.code()), Some(Code::Denied));

    let mut first = fixture.client.subscribe(authorized(subscription.clone(), &gateway)).await.unwrap().into_inner();
    let snapshot = next(&mut first).await;
    assert!(snapshot.snapshot && !snapshot.stream.is_empty());
    let after = Cursor { stream: snapshot.stream.clone(), position: snapshot.position };
    let resumed = SubscribeRequest { after: Some(after), ..subscription };
    let mut second = fixture.client.subscribe(authorized(resumed, &gateway)).await.unwrap().into_inner();
    let update = next(&mut second).await;
    assert!(!update.snapshot && update.error.is_none());
    assert!(!update.stream.is_empty() && update.stream != snapshot.stream);

    let mut last = next(&mut first).await;
    while last.error.is_none() {
        last = next(&mut first).await;
    }
    assert_eq!(last.error.map(|error| error.code()), Some(Code::Stopped));
    assert!(first.message().await.unwrap().is_none());
    assert_eq!(code(&fixture.call_on(&gateway, &snapshot.stream).await), Code::Stopped);
    let current = fixture.call_on(&gateway, &update.stream).await;
    assert_eq!(current.outcome, Some(Outcome::Result(b"0".to_vec())));
    let cli = fixture.cli.clone();
    assert_eq!(code(&fixture.call_on(&cli, &update.stream).await), Code::Stopped);
    drop((first, second));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gateway_acts_only_for_players_it_holds_claims_for() {
    let (jvm, server) = runtime::Runtime::start();
    let mut fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    fixture.control.activate_release(runtime::release()).unwrap();
    fixture.control.claim(runtime::login()).await.unwrap();
    let holder = fixture.gateway.clone();
    let other = fixture.gateways.mint("other");
    let player = || Some(Caller { session: String::new(), player: runtime::PLAYER.into() });

    let held = fixture.call_as(&holder, "", "get", "null", player()).await;
    assert_eq!(held.outcome, Some(Outcome::Result(b"0".to_vec())));
    assert_eq!(code(&fixture.call_as(&other, "", "get", "null", player()).await), Code::Denied);

    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"count": {"function": "get"}}"#.to_vec(),
        deployment: "test".into(),
        caller: player(),
        ..SubscribeRequest::default()
    };
    let mut held = fixture.client.subscribe(authorized(subscription.clone(), &holder)).await.unwrap().into_inner();
    assert!(next(&mut held).await.snapshot);
    let mut foreign = fixture.client.subscribe(authorized(subscription, &other)).await.unwrap().into_inner();
    assert_eq!(next(&mut foreign).await.error.map(|error| error.code()), Some(Code::Denied));
    drop((held, foreign));
    fixture.stop().await;
    server.abort();
}
