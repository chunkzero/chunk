use std::{collections::BTreeMap, time::Duration};

use chunk_contract::{
    Contracts, Deployment, DomainManifest, Function, FunctionKind, RuntimeProfile, Schema, Visibility,
};
use chunk_js::{DeploymentId, Json};
use chunk_proto::v1::{CommandCatalog, CommandScope, CommandSuggestionRequest};
use chunk_store::SqliteStore;
use serde_json::json;

use super::*;
use crate::{ActionHandle, Backend, Call, Error};

mod compiled;

const COMMAND: &str = "scopes/commands/notify";
const GATEWAY: &str = "gateway-credential";
fn scope() -> CommandScope {
    CommandScope {
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
fn caller() -> Json {
    json!({"kind":"gateway","player":"alice"}).into()
}
fn id() -> DeploymentId {
    DeploymentId::new("commands").unwrap()
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
export function permit(ctx) { return ctx.caller.kind === 'gateway' && ctx.caller.player === 'alice' && (ctx.db.get('state','denied')?.value ?? 0)===0; }
export function choices() {return ['one','two'];}
export function read(ctx) {return ctx.db.get('state','count')?.value ?? 0;}
export function count(ctx) {const value=read(ctx)+1;ctx.db.put('state','count',{value});return value;}
export function revoke(ctx,args) {ctx.db.put('state','denied',{value:args});return null;}
export async function notify(ctx,args) {await ctx.runMutation('count',{}); if(args.arguments.text==='session') { const result=await ctx.platform({kind:'session_call',method:{app:'lobby',session:'main',name:'status'},arguments:{limit:1}}); if(result!==7) throw Error('unexpected result'); return null; } if(args.arguments.text==='fanout') { await Promise.all(Array.from({length:8},()=>ctx.platform({kind:'message',text:'fanout'}))); return null; } if(args.arguments.text==='wait') await ctx.sleep(400); await ctx.platform({kind:'message',text:args.arguments.text});return null;}
export function hidden() {return null;}
export async function ambient(ctx) {await ctx.platform({kind:'message',text:'forbidden'});return null;}
".into(),
        functions:[("permit",FunctionKind::Query,Visibility::Internal,empty.clone(),Schema::Boolean),("choices",FunctionKind::Query,Visibility::Internal,suggestion,Schema::Array {items:Box::new(Schema::String)}),("read",FunctionKind::Query,Visibility::Public,empty.clone(),Schema::Integer),("count",FunctionKind::Mutation,Visibility::Internal,empty.clone(),Schema::Integer),("revoke",FunctionKind::Mutation,Visibility::Public,Schema::Integer,Schema::Null),("ambient",FunctionKind::Action,Visibility::Public,empty,Schema::Null)].into_iter().map(|(name,kind,visibility,arguments,result)|(name.into(),Function {kind,visibility,export:name.into(),arguments,result})).collect(),
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    backend: Backend,
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
        Self { _directory: directory, backend }
    }
    async fn catalog(&self, scope: CommandScope) -> crate::Result<CommandCatalog> {
        self.backend.command_catalog(id(), scope, caller()).await
    }
    async fn suggest(&self, query: &str, input: &str) -> crate::Result<Vec<String>> {
        let cursor = u32::try_from(input.len()).unwrap();
        let request = CommandSuggestionRequest {
            scope: Some(scope()),
            command_id: COMMAND.into(),
            query: query.into(),
            input: input.into(),
            cursor,
        };
        let charge = self.backend.charge_request(0).unwrap();
        Ok(self.backend.command_suggestions(id(), charge, request, caller()).await?.values)
    }
    /// Starts the command with `input` for `alice`, as core's sync path does.
    async fn start(&self, input: &str) -> crate::Result<(ActionHandle, CommandEffects)> {
        let action = self.backend.allocate_action_id().await.unwrap();
        let charge = self.backend.charge_request(0).unwrap();
        let request = CommandRequest::new(COMMAND, input, "alice");
        let (command, input, caller) = (COMMAND.into(), input.into(), caller());
        self.backend.start_command(action, charge, GATEWAY, request, id(), scope(), command, input, caller).await
    }
    async fn count(&self) -> i64 {
        let read =
            Call { deployment: id(), function: "read".into(), arguments: json!({}).into(), caller: json!(null).into() };
        serde_json::from_str(&self.backend.query(read).await.unwrap().json).unwrap()
    }
    async fn revoke(&self, operation: &str, value: i64) {
        let call = Call {
            deployment: id(),
            function: "revoke".into(),
            arguments: json!(value).into(),
            caller: json!(null).into(),
        };
        self.backend.mutate(operation.into(), call).await.unwrap();
    }
}
async fn effect(effects: &mut CommandEffects) -> CommandEffect {
    tokio::time::timeout(Duration::from_secs(3), effects.recv()).await.unwrap().expect("an effect")
}
async fn outcome(action: &mut ActionHandle) -> crate::Result<Arc<str>> {
    tokio::time::timeout(Duration::from_secs(3), action.outcome()).await.unwrap()
}
fn request(effect: &CommandEffect) -> serde_json::Value {
    serde_json::from_str(effect.request().as_str()).unwrap()
}

#[tokio::test]
async fn catalog_suggestions_and_starts_check_the_scope_permission_and_declared_queries() {
    let fixture = Fixture::new().await;
    assert_eq!(fixture.catalog(scope()).await.unwrap().allowed_ids, [COMMAND]);
    let mut wrong = scope();
    wrong.domain = "private".into();
    assert!(fixture.catalog(wrong).await.is_err());
    let mut unknown_app = scope();
    unknown_app.app = "missing".into();
    unknown_app.session_type = "missing/main".into();
    assert!(fixture.catalog(unknown_app).await.is_err());
    let mut large_generation = scope();
    large_generation.membership_generation = u64::MAX;
    large_generation.delivery_generation = u64::MAX;
    assert_eq!(fixture.catalog(large_generation).await.unwrap().allowed_ids, [COMMAND]);
    assert_eq!(fixture.suggest("choices", "notify o").await.unwrap(), ["one", "two"]);
    assert!(fixture.suggest("read", "").await.is_err());
    // The input can't name another command than the one started.
    assert!(fixture.start("private").await.is_err());
    fixture.revoke("permission-1", 1).await;
    let hidden = fixture.catalog(scope()).await.unwrap();
    assert!(hidden.allowed_ids.is_empty());
    assert!(serde_json::from_slice::<serde_json::Value>(&hidden.commands_json).unwrap().get(COMMAND).is_some());
}

#[tokio::test]
async fn permission_is_fresh_at_dispatch_and_before_later_effects() {
    let fixture = Fixture::new().await;
    fixture.revoke("permission-1", 1).await;
    assert!(matches!(fixture.start("notify hello").await, Err(Error::Unknown)));
    assert_eq!(fixture.count().await, 0);
    fixture.revoke("permission-0", 0).await;
    let (mut action, _effects) = fixture.start("notify wait").await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.count().await == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    fixture.revoke("permission-1-again", 1).await;
    assert!(outcome(&mut action).await.is_err());
    assert_eq!(fixture.count().await, 1);
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
}

#[tokio::test]
async fn session_calls_validate_declared_reply_contract_before_resuming_handler() {
    let fixture = Fixture::new().await;
    for (value, succeeds) in [(json!(7), true), (json!("7"), false)] {
        let (mut action, mut effects) = fixture.start("notify session").await.unwrap();
        let call = effect(&mut effects).await;
        assert_eq!(
            request(&call),
            json!({"kind":"session_call","method":{"app":"lobby","session":"main","name":"status"},"arguments":{"limit":1}})
        );
        call.finish(Some(&serde_json::to_vec(&value).unwrap()));
        assert_eq!(outcome(&mut action).await.is_ok(), succeeds);
    }
    assert_eq!(fixture.count().await, 2);
}

#[tokio::test]
async fn larger_action_budgets_hold_more_platform_effects_in_flight() {
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("commands.db"), "test").unwrap();
    // Sixteen live actions, with four transaction or platform effects each.
    let budget = 16 * chunk_js::Limits::default().heap_bytes;
    let effects = crate::ActionEffects::new("test".into()).unwrap();
    let backend = Backend::with_action_bytes("test".into(), Box::new(store), effects, budget).unwrap();
    backend.deploy(deployment()).await.unwrap();
    let fixture = Fixture { _directory: directory, backend };
    let mut runs = Vec::new();
    for _ in 0..5 {
        let (action, mut effects) = fixture.start("notify fanout").await.unwrap();
        let mut pending = Vec::new();
        for _ in 0..8 {
            pending.push(effect(&mut effects).await);
        }
        runs.push((action, effects, pending));
    }
    // Forty effects are unanswered at once, past the 32 the smallest budget admits.
    for (mut action, _effects, pending) in runs {
        for effect in pending {
            effect.accept();
        }
        assert!(outcome(&mut action).await.is_ok());
    }
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

#[tokio::test]
async fn command_catalog_admission_charges_retained_scope_and_caller() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let bytes = scope_bytes(&scope()) + caller().as_str().len();
    let mut catalog = Box::pin(backend.command_catalog(id(), scope(), caller()));
    crate::tests::pending(catalog.as_mut()).await;
    let charged = crate::limits::REQUEST_BYTES - memory.available_permits();
    let required = crate::limits::REQUEST_OVERHEAD + bytes;
    assert!(charged >= required, "retained catalog charged {charged} bytes, but needs at least {required} bytes");
}

#[tokio::test]
async fn command_suggestion_admission_charges_retained_scope_input_and_caller() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let request = CommandSuggestionRequest {
        scope: Some(scope()),
        command_id: COMMAND.into(),
        query: "choices".into(),
        input: "notify o".into(),
        cursor: 8,
    };
    let bytes = scope_bytes(&scope())
        + request.command_id.len()
        + request.query.len()
        + request.input.len()
        + caller().as_str().len();
    let charge = backend.charge_request(0).unwrap();
    let mut suggestions = Box::pin(backend.command_suggestions(id(), charge, request, caller()));
    crate::tests::pending(suggestions.as_mut()).await;
    let charged = crate::limits::REQUEST_BYTES - memory.available_permits();
    let required = crate::limits::REQUEST_OVERHEAD + bytes;
    assert!(charged >= required, "retained suggestion charged {charged} bytes, but needs at least {required} bytes");
}

#[tokio::test]
async fn command_start_admission_charges_retained_call_scope_input_and_caller() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let action = crate::ActionId { incarnation: "test-incarnation".into(), sequence: 1 };
    let input = "notify hello";
    let call = Call {
        deployment: id(),
        function: COMMAND.into(),
        arguments: json!({"input": input}).into(),
        caller: caller(),
    };
    let bytes = action.incarnation.len() + call.bytes() + scope_bytes(&scope()) + input.len() + caller().as_str().len();
    let request = CommandRequest::new(COMMAND, input, "alice");
    let charge = backend.charge_request(0).unwrap();
    let mut started = Box::pin(backend.start_command(
        action,
        charge,
        GATEWAY,
        request,
        id(),
        scope(),
        COMMAND.into(),
        input.into(),
        caller(),
    ));
    crate::tests::pending(started.as_mut()).await;
    let charged = crate::limits::REQUEST_BYTES - memory.available_permits();
    let required = crate::limits::REQUEST_OVERHEAD + bytes;
    assert!(charged >= required, "retained command charged {charged} bytes, but needs at least {required} bytes");
}
