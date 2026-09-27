use std::{collections::BTreeMap, time::Duration};

use chunk_contract::{
    Contracts, Deployment, DomainManifest, Function, FunctionKind, RuntimeProfile, Schema, Visibility,
};
use chunk_js::DeploymentId;
use chunk_proto::v1::{
    self as wire, backend_commands_client::BackendCommandsClient, backend_commands_server::BackendCommands,
    command_client_frame, command_server_frame,
};
use chunk_store::SqliteStore;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tokio_util::sync::CancellationToken;
use tonic::Request;

use super::*;
use crate::{Backend, Call, Error};

mod compiled;

const APPLICATION: &str = "application-credential-is-not-platform";
const PLATFORM: &str = "platform-command-credential-is-distinct";
const COMMAND: &str = "scopes/commands/notify";
fn scope() -> wire::CommandScope {
    wire::CommandScope {
        proxy_id: "proxy".into(),
        player_uuid: "alice".into(),
        username: "Alice".into(),
        session_id: "session-one".into(),
        app: "lobby".into(),
        session_type: "lobby/main".into(),
        domain: String::new(),
        scope_id: "scope-one".into(),
        connection_id: "connection-one".into(),
        claim_operation_id: "claim-one".into(),
        membership_generation: 1,
        delivery_generation: 1,
    }
}
fn request<T>(message: T, credential: &str) -> Request<T> {
    let mut request = Request::new(message);
    for (name, value) in [
        ("authorization", format!("Bearer {credential}")),
        ("x-chunk-environment", "test".into()),
        ("x-chunk-deployment", "commands".into()),
    ] {
        request.metadata_mut().insert(name, value.parse().unwrap());
    }
    request
}
fn deployment() -> Deployment {
    let domains:DomainManifest=serde_json::from_value(json!({"version":1,"scopes":{"":{"parent":null},"private":{"parent":""}},"apps":{"lobby":""},"hooks":{},"commands":{
        COMMAND:{"domain":"","name":"notify","aliases":["n"],"export":"notify","permission":"permit","follow_player":true,"routes":[{"literals":[],"arguments":[{"name":"text","parser":"word","suggestions":{"query":"choices"}}]}]},
        "scopes/private/commands/hidden":{"domain":"private","name":"private","aliases":[],"export":"hidden","follow_player":false,"routes":[{"literals":[],"arguments":[]}]}
    }})).unwrap();
    let empty = Schema::Object { fields: BTreeMap::new() };
    let suggestion = serde_json::from_value(
        json!({"type":"object","fields":{"input":{"schema":{"type":"string"}},"cursor":{"schema":{"type":"integer"}}}}),
    )
    .unwrap();
    Deployment {contract_version:2,runtime_profile:RuntimeProfile::TransactionalV1,id:"commands".into(),contracts:Contracts{domains:Some(domains),session_methods:Some(serde_json::from_value(json!({"version":1,"methods":[{"app":"lobby","session":"main","name":"status","arguments":{"type":"object","fields":{"limit":{"schema":{"type":"integer"}}}},"result":{"type":"integer"}}]})).unwrap()),..Default::default()},
        tables:serde_json::from_value(json!({"state":{"fields":{"value":{"schema":{"type":"integer"}}}}})).unwrap(),
        source:r"
export function permit(ctx) { return ctx.caller.kind === 'command' && ctx.caller.player === 'alice' && ctx.caller.claimOperationId === 'claim-one' && (ctx.db.get('state','denied')?.value ?? 0)===0; }
export function choices() {return ['one','two'];}
export function read(ctx) {return ctx.db.get('state','count')?.value ?? 0;}
export function count(ctx) {const value=read(ctx)+1;ctx.db.put('state','count',{value});return value;}
export function revoke(ctx,args) {ctx.db.put('state','denied',{value:args});return null;}
export async function notify(ctx,args) {await ctx.runMutation('count',{}); if(args.arguments.text==='session') { const result=await ctx.platform({kind:'session_call',method:{app:'lobby',session:'main',name:'status'},arguments:{limit:1}}); if(result!==7) throw Error('unexpected result'); return null; } if(args.arguments.text==='wait') await ctx.sleep(400); await ctx.platform({kind:'message',text:args.arguments.text});return null;}
export function hidden() {return null;}
export async function ambient(ctx) {await ctx.platform({kind:'message',text:'forbidden'});return null;}
".into(),
        functions:[("permit",FunctionKind::Query,Visibility::Internal,empty.clone(),Schema::Boolean),("choices",FunctionKind::Query,Visibility::Internal,suggestion,Schema::Array {items:Box::new(Schema::String)}),("read",FunctionKind::Query,Visibility::Public,empty.clone(),Schema::Integer),("count",FunctionKind::Mutation,Visibility::Internal,empty.clone(),Schema::Integer),("revoke",FunctionKind::Mutation,Visibility::Public,Schema::Integer,Schema::Null),("ambient",FunctionKind::Action,Visibility::Public,empty,Schema::Null)].into_iter().map(|(name,kind,visibility,arguments,result)|(name.into(),Function {kind,visibility,export:name.into(),arguments,result})).collect(),
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    backend: Backend,
    service: CommandService,
    client: BackendCommandsClient<tonic::transport::Channel>,
    stop: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_deployment(deployment()).await
    }
    async fn with_deployment(deployment: Deployment) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new(
            "test".into(),
            Box::new(SqliteStore::open(directory.path().join("commands.db"), "test").unwrap()),
        )
        .unwrap();
        backend.deploy(deployment).await.unwrap();
        Self::with_backend(directory, backend).await
    }
    async fn with_backend(directory: tempfile::TempDir, backend: Backend) -> Self {
        let service = CommandService::new(backend.clone(), APPLICATION, PLATFORM).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let stop = CancellationToken::new();
        let server = tokio::spawn({
            let service = service.clone();
            let stop = stop.clone();
            async move {
                tonic::transport::Server::builder()
                    .add_service(service.into_server())
                    .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop.cancelled_owned())
                    .await
                    .unwrap();
            }
        });
        let client = BackendCommandsClient::connect(endpoint).await.unwrap();
        Self { _directory: directory, backend, service, client, stop, server }
    }
    async fn prepare(&mut self, input: &str) -> wire::PreparedCommand {
        self.client
            .prepare(request(
                wire::PrepareCommand { scope: Some(scope()), command_id: COMMAND.into(), input: input.into() },
                PLATFORM,
            ))
            .await
            .unwrap()
            .into_inner()
    }
    async fn run(
        &mut self,
        id: &str,
    ) -> (mpsc::Sender<wire::CommandClientFrame>, tonic::Streaming<wire::CommandServerFrame>) {
        let (sender, receiver) = mpsc::channel(8);
        sender
            .send(wire::CommandClientFrame {
                frame: Some(command_client_frame::Frame::Start(wire::CommandStart { invocation_id: id.into() })),
            })
            .await
            .unwrap();
        let stream = self.client.run(request(ReceiverStream::new(receiver), PLATFORM)).await.unwrap().into_inner();
        (sender, stream)
    }
    async fn count(&self) -> i64 {
        serde_json::from_str(
            &self
                .backend
                .query(Call {
                    deployment: DeploymentId::new("commands").unwrap(),
                    function: "read".into(),
                    arguments: json!({}).into(),
                    caller: json!(null).into(),
                })
                .await
                .unwrap()
                .json,
        )
        .unwrap()
    }
    async fn revoke(&self, value: i64) {
        self.backend
            .mutate(
                format!("permission-{value}"),
                Call {
                    deployment: DeploymentId::new("commands").unwrap(),
                    function: "revoke".into(),
                    arguments: json!(value).into(),
                    caller: json!(null).into(),
                },
            )
            .await
            .unwrap();
    }
    async fn close(&self) {
        self.service.shutdown().cancel();
        self.service.workers().close();
        tokio::time::timeout(Duration::from_secs(2), self.service.workers().wait()).await.unwrap();
        self.stop.cancel();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.service.shutdown().cancel();
        self.stop.cancel();
        self.server.abort();
    }
}
async fn frame(stream: &mut tonic::Streaming<wire::CommandServerFrame>) -> command_server_frame::Frame {
    tokio::time::timeout(Duration::from_secs(3), stream.message()).await.unwrap().unwrap().unwrap().frame.unwrap()
}
fn completion(frame: command_server_frame::Frame) -> wire::CommandCompletionState {
    let command_server_frame::Frame::Finished(value) = frame else { panic!("expected completion") };
    wire::CommandCompletionState::try_from(value.state).unwrap()
}

#[tokio::test]
async fn command_authority_scope_queries_and_fixed_descriptor_are_checked() {
    let mut fixture = Fixture::new().await;
    assert_eq!(
        fixture.service.catalog(request(scope(), APPLICATION)).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    let catalog = fixture.service.catalog(request(scope(), PLATFORM)).await.unwrap().into_inner();
    assert_eq!(catalog.allowed_ids, [COMMAND]);
    let mut wrong = scope();
    wrong.domain = "private".into();
    assert!(fixture.service.catalog(request(wrong, PLATFORM)).await.is_err());
    let mut unknown_app = scope();
    unknown_app.app = "missing".into();
    unknown_app.session_type = "missing/main".into();
    assert!(fixture.service.catalog(request(unknown_app, PLATFORM)).await.is_err());
    let mut large_generation = scope();
    large_generation.membership_generation = u64::MAX;
    large_generation.delivery_generation = u64::MAX;
    assert_eq!(
        fixture.service.catalog(request(large_generation, PLATFORM)).await.unwrap().into_inner().allowed_ids,
        [COMMAND]
    );
    let suggestions = fixture
        .service
        .suggest(request(
            wire::CommandSuggestionRequest {
                scope: Some(scope()),
                command_id: COMMAND.into(),
                query: "choices".into(),
                input: "notify o".into(),
                cursor: 8,
            },
            PLATFORM,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(suggestions.values, ["one", "two"]);
    assert!(
        fixture
            .service
            .suggest(request(
                wire::CommandSuggestionRequest {
                    scope: Some(scope()),
                    command_id: COMMAND.into(),
                    query: "read".into(),
                    input: String::new(),
                    cursor: 0
                },
                PLATFORM
            ))
            .await
            .is_err()
    );
    assert!(
        fixture
            .client
            .prepare(request(
                wire::PrepareCommand { scope: Some(scope()), command_id: COMMAND.into(), input: "private".into() },
                PLATFORM
            ))
            .await
            .is_err()
    );
    fixture.revoke(1).await;
    let hidden = fixture.service.catalog(request(scope(), PLATFORM)).await.unwrap().into_inner();
    assert!(hidden.allowed_ids.is_empty());
    assert!(serde_json::from_slice::<serde_json::Value>(&hidden.commands_json).unwrap().get(COMMAND).is_some());
    fixture.close().await;
}

#[tokio::test]
async fn duplicate_command_streams_observe_without_reexecuting_or_replaying_effects() {
    let mut fixture = Fixture::new().await;
    let prepared = fixture.prepare("n hello").await;
    assert!(prepared.follow_player);
    let (owner, mut output) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(
        frame(&mut output).await,
        command_server_frame::Frame::Accepted(wire::CommandAccepted { status_only: false, .. })
    ));
    let command_server_frame::Frame::Effect(effect) = frame(&mut output).await else { panic!("effect") };
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&effect.request_json).unwrap(),
        json!({"kind":"message","text":"hello"})
    );
    assert_eq!(effect.operation_id, format!("action/{}/platform/{}", prepared.invocation_id, effect.sequence));
    assert!(matches!(fixture.backend.release(DeploymentId::new("commands").unwrap()).await, Err(Error::Busy)));
    let (_observer, mut replay) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(
        frame(&mut replay).await,
        command_server_frame::Frame::Accepted(wire::CommandAccepted { status_only: true, .. })
    ));
    owner
        .send(wire::CommandClientFrame {
            frame: Some(command_client_frame::Frame::Reply(wire::CommandEffectReply {
                sequence: effect.sequence,
                result_json: serde_json::to_vec(&json!({"state":"accepted","operationId":effect.operation_id}))
                    .unwrap(),
                error: String::new(),
            })),
        })
        .await
        .unwrap();
    assert_eq!(completion(frame(&mut output).await), wire::CommandCompletionState::Succeeded);
    assert_eq!(completion(frame(&mut replay).await), wire::CommandCompletionState::Succeeded);
    assert_eq!(fixture.count().await, 1);
    let (_observer, mut replay) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(frame(&mut replay).await, command_server_frame::Frame::Accepted(_)));
    assert_eq!(completion(frame(&mut replay).await), wire::CommandCompletionState::Succeeded);
    assert_eq!(fixture.count().await, 1);
    fixture.close().await;
}

#[tokio::test]
async fn permission_is_fresh_at_dispatch_and_before_later_effects() {
    let mut fixture = Fixture::new().await;
    let prepared = fixture.prepare("notify hello").await;
    fixture.revoke(1).await;
    let (_sender, mut output) = fixture.run(&prepared.invocation_id).await;
    assert_eq!(completion(frame(&mut output).await), wire::CommandCompletionState::Failed);
    assert_eq!(fixture.count().await, 0);
    fixture.revoke(0).await;
    let prepared = fixture.prepare("notify wait").await;
    let (_sender, mut output) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Accepted(_)));
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.count().await == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // Use a distinct operation identity for a new revocation.
    fixture
        .backend
        .mutate(
            "revoked-again".into(),
            Call {
                deployment: DeploymentId::new("commands").unwrap(),
                function: "revoke".into(),
                arguments: json!(1).into(),
                caller: json!(null).into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(completion(frame(&mut output).await), wire::CommandCompletionState::Failed);
    assert_eq!(fixture.count().await, 1);
    fixture.close().await;
}

#[tokio::test]
async fn owner_disconnect_and_shutdown_cancel_unanswered_effects_with_unknown_outcomes() {
    let mut fixture = Fixture::new().await;
    let prepared = fixture.prepare("notify hello").await;
    let (owner, mut output) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Accepted(_)));
    assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Effect(_)));
    drop(owner);
    assert_eq!(completion(frame(&mut output).await), wire::CommandCompletionState::Unknown);
    let (_observer, mut replay) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(frame(&mut replay).await, command_server_frame::Frame::Accepted(_)));
    assert_eq!(completion(frame(&mut replay).await), wire::CommandCompletionState::Unknown);
    let prepared = fixture.prepare("notify shutdown").await;
    let (_owner, mut output) = fixture.run(&prepared.invocation_id).await;
    assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Accepted(_)));
    assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Effect(_)));
    fixture.close().await;
    assert_eq!(completion(frame(&mut output).await), wire::CommandCompletionState::Unknown);
    assert_eq!(fixture.count().await, 2);
}

#[tokio::test]
async fn regular_actions_do_not_inherit_platform_capabilities() {
    let fixture = Fixture::new().await;
    let mut action = fixture
        .backend
        .start_action(
            fixture.backend.allocate_action_id().await.unwrap(),
            Call {
                deployment: DeploymentId::new("commands").unwrap(),
                function: "ambient".into(),
                arguments: json!({}).into(),
                caller: json!(null).into(),
            },
        )
        .await
        .unwrap();
    assert!(action.outcome().await.unwrap_err().to_string().contains("platform capability unavailable"));
    fixture.close().await;
}

#[tokio::test]
async fn session_calls_validate_declared_reply_contract_before_resuming_handler() {
    let mut fixture = Fixture::new().await;
    for (value, expected) in
        [(json!(7), wire::CommandCompletionState::Succeeded), (json!("7"), wire::CommandCompletionState::Failed)]
    {
        let prepared = fixture.prepare("notify session").await;
        let (sender, mut output) = fixture.run(&prepared.invocation_id).await;
        assert!(matches!(frame(&mut output).await, command_server_frame::Frame::Accepted(_)));
        let command_server_frame::Frame::Effect(effect) = frame(&mut output).await else { panic!("session call") };
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&effect.request_json).unwrap(),
            json!({"kind":"session_call","method":{"app":"lobby","session":"main","name":"status"},"arguments":{"limit":1}})
        );
        sender
            .send(wire::CommandClientFrame {
                frame: Some(command_client_frame::Frame::Reply(wire::CommandEffectReply {
                    sequence: effect.sequence,
                    result_json: serde_json::to_vec(&value).unwrap(),
                    error: String::new(),
                })),
            })
            .await
            .unwrap();
        assert_eq!(completion(frame(&mut output).await), expected);
    }
    assert_eq!(fixture.count().await, 2);
    fixture.close().await;
}

#[test]
fn platform_requests_reject_foreign_targets_undeclared_methods_and_oversized_values() {
    let deployment = deployment();
    let session = json!({"kind":"session_call","method":{"app":"lobby","session":"main","name":"status"},"arguments":{"limit":1}});
    assert!(effects::validate(&deployment, &scope(), &session.clone().into()).is_ok());
    for bad in [
        json!({"kind":"message","text":"hello","player":"mallory"}),
        json!({"kind":"message","text":"x".repeat(4097)}),
        json!({"kind":"enter","destination":{"key":"k","session_type":"lobby/main","machine_profile":"m","player":"mallory"}}),
    ] {
        assert!(effects::validate(&deployment, &scope(), &bad.into()).is_err());
    }
    for (field, value) in [("app", "other"), ("session", "other"), ("name", "undeclared")] {
        let mut bad = session.clone();
        bad["method"][field] = json!(value);
        assert!(effects::validate(&deployment, &scope(), &bad.into()).is_err());
    }
    let mut bad = session;
    bad["arguments"]["limit"] = json!("wrong type");
    assert!(effects::validate(&deployment, &scope(), &bad.into()).is_err());
}

fn scope_bytes(scope: &wire::CommandScope) -> usize {
    [
        &scope.proxy_id,
        &scope.player_uuid,
        &scope.username,
        &scope.session_id,
        &scope.app,
        &scope.session_type,
        &scope.domain,
        &scope.scope_id,
        &scope.connection_id,
        &scope.claim_operation_id,
    ]
    .iter()
    .map(|value| value.len())
    .sum()
}

fn assert_ingress_charge(memory: &tokio::sync::Semaphore, payload: usize) {
    let charged = crate::limits::REQUEST_BYTES - memory.available_permits();
    let required = crate::limits::REQUEST_OVERHEAD + payload;
    assert!(charged >= required, "retained command charged {charged} bytes, but needs at least {required} bytes");
}

#[tokio::test]
async fn command_catalog_admission_charges_retained_scope() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let service = CommandService::new(backend, APPLICATION, PLATFORM).unwrap();
    let scope = scope();
    let bytes = scope_bytes(&scope);
    let mut catalog = Box::pin(service.catalog(request(scope, PLATFORM)));
    crate::tests::pending(catalog.as_mut()).await;
    assert_ingress_charge(&memory, bytes);
}

#[tokio::test]
async fn command_suggestion_admission_charges_retained_scope_and_input() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let service = CommandService::new(backend, APPLICATION, PLATFORM).unwrap();
    let scope = scope();
    let suggestion = wire::CommandSuggestionRequest {
        scope: Some(scope.clone()),
        command_id: COMMAND.into(),
        query: "choices".into(),
        input: "notify o".into(),
        cursor: 8,
    };
    let bytes = scope_bytes(&scope) + suggestion.command_id.len() + suggestion.query.len() + suggestion.input.len();
    let mut suggestions = Box::pin(service.suggest(request(suggestion, PLATFORM)));
    crate::tests::pending(suggestions.as_mut()).await;
    assert_ingress_charge(&memory, bytes);
}

#[tokio::test]
async fn command_preparation_admission_charges_retained_scope_and_input() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let service = CommandService::new(backend, APPLICATION, PLATFORM).unwrap();
    let scope = scope();
    let preparation =
        wire::PrepareCommand { scope: Some(scope.clone()), command_id: COMMAND.into(), input: "notify hello".into() };
    let bytes = scope_bytes(&scope) + preparation.command_id.len() + preparation.input.len();
    let mut prepared = Box::pin(service.prepare(request(preparation, PLATFORM)));
    crate::tests::pending(prepared.as_mut()).await;
    assert_ingress_charge(&memory, bytes);
}

#[tokio::test]
async fn command_start_admission_charges_retained_scope_and_input() {
    use crate::service::{Command, Event};

    let (backend, mut incoming, memory) = Backend::held_ingress();
    let mut fixture = Fixture::with_backend(tempfile::tempdir().unwrap(), backend).await;
    let scope = scope();
    let input = "notify hello";
    let mut preparation = Box::pin(fixture.service.prepare(request(
        wire::PrepareCommand { scope: Some(scope.clone()), command_id: COMMAND.into(), input: input.into() },
        PLATFORM,
    )));
    crate::tests::pending(preparation.as_mut()).await;
    let Event::Request { command, .. } = incoming.try_recv().unwrap() else { panic!("expected preparation") };
    let Command::Prepare { id, scope, command, input, reply } = *command else { panic!("expected preparation") };
    let prepared = Prepared { deployment: id, scope, command, input, follow_player: false };
    let bytes = prepared.call().bytes() + scope_bytes(&prepared.scope) + prepared.input.len();
    reply.finish(Ok(prepared));
    crate::tests::pending(preparation.as_mut()).await;
    let Event::Request { command, .. } = incoming.try_recv().unwrap() else { panic!("expected an action identity") };
    let Command::PrepareAction { reply } = *command else { panic!("expected an action identity") };
    reply.finish(Ok(crate::ActionId { incarnation: "test-incarnation".into(), sequence: 1 }));
    let invocation = preparation.await.unwrap().into_inner().invocation_id;
    let (_sender, _output) = fixture.run(&invocation).await;
    let event = tokio::time::timeout(Duration::from_secs(2), incoming.recv()).await.unwrap().unwrap();
    assert!(
        matches!(&event, Event::Request { command, .. } if matches!(command.as_ref(), Command::StartAction { .. }))
    );
    // Close the transport worker before asserting; the retained event still owns its permit.
    fixture.close().await;
    assert_ingress_charge(&memory, bytes);
}
