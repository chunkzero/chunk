use std::{
    collections::BTreeSet,
    fmt::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use crate::PlatformTarget;
use chunk_backend::{Backend, Call};
use chunk_contract::{Contracts, Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::{DeploymentId, Json};
use chunk_proto::sync::v1::{
    self as sync, CallResponse, PlayerIdentity, PrepareResult, SubscribeRequest, Update, call_response,
    core_server::{Core, CoreServer},
    error::Code,
};
use chunk_store::SqliteStore;
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response};

use super::*;

/// A fake core that reads domain manifests from `backend` and runs its hooks as core does: each under an operation ID
/// `chunk:prepare` issued, within its call, whose drop cancels it.
#[derive(Clone)]
struct Hooks {
    backend: Backend,
    prepared: Arc<Mutex<BTreeSet<String>>>,
    /// Each hook run, with the caller app code received.
    callers: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Hooks {
    async fn run(&self, call: CallRequest) -> Result<Vec<u8>, sync::Error> {
        assert!(call.stream.is_empty());
        let failed =
            |error: chunk_backend::Error| sync::Error { code: Code::Application.into(), message: error.to_string() };
        if call.method == "chunk:prepare" {
            let id = self.backend.allocate_action_id().await.map_err(failed)?;
            let operation_id = format!("prep:{id}");
            self.prepared.lock().unwrap().insert(operation_id.clone());
            return Ok(PrepareResult { operation_id }.encode_to_vec());
        }
        let deployment = DeploymentId::new(&call.deployment).unwrap();
        if call.method == "chunk:manifest" {
            let manifest = self.backend.domain_manifest(deployment).await.map_err(failed)?.unwrap();
            let manifest_json = serde_json::to_vec(&manifest).unwrap();
            return Ok(ManifestResult { deployment: call.deployment, manifest_json }.encode_to_vec());
        }
        // Each prepared operation ID runs one hook.
        assert!(self.prepared.lock().unwrap().remove(&call.operation_id));
        let id = call.operation_id.strip_prefix("prep:").unwrap().parse().unwrap();
        let mut caller = json!({"kind": "gateway"});
        if let Some(named) = &call.caller {
            caller["player"] = named.player.clone().into();
        }
        self.callers.lock().unwrap().push((call.method.clone(), caller.clone()));
        let arguments = Json::parse(std::str::from_utf8(&call.arguments).unwrap()).unwrap();
        let call = Call { deployment, function: call.method, arguments, caller: caller.into() };
        let mut handle = self.backend.start_hook(id, call).await.map_err(failed)?;
        Ok(handle.outcome().await.map_err(failed)?.as_bytes().to_vec())
    }
}

#[tonic::async_trait]
impl Core for Hooks {
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, tonic::Status> {
        assert_eq!(request.metadata().get("authorization").unwrap(), "Bearer gateway");
        let outcome = match self.run(request.into_inner()).await {
            Ok(result) => call_response::Outcome::Result(result),
            Err(error) => call_response::Outcome::Error(error),
        };
        Ok(Response::new(CallResponse { position: None, outcome: Some(outcome) }))
    }

    type SubscribeStream = ReceiverStream<Result<Update, tonic::Status>>;
    async fn subscribe(&self, _: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("unused"))
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    hooks: Hooks,
    platform: Platform,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new(
            "test".into(),
            Box::new(SqliteStore::open(directory.path().join("hooks.db"), "test").unwrap()),
        )
        .unwrap();
        backend.deploy(deployment("candidate", false)).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let platform = Platform::new(PlatformTarget {
            core: format!("http://{}", listener.local_addr().unwrap()),
            gateway: crate::GatewayCredential { id: "proxy".into(), credential: "gateway".into() },
            deployment: "candidate".into(),
        })
        .unwrap();
        let hooks = Hooks { backend, prepared: Arc::default(), callers: Arc::default() };
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(CoreServer::new(hooks.clone()))
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        Self { _directory: directory, hooks, platform, server }
    }

    fn call(name: &str) -> Call {
        Call {
            deployment: DeploymentId::new("candidate").unwrap(),
            function: name.into(),
            arguments: Value::Null.into(),
            caller: Value::Null.into(),
        }
    }

    async fn trace(&self) -> String {
        serde_json::from_str(&self.hooks.backend.query(Self::call("trace")).await.unwrap().json).unwrap()
    }

    /// The player each hook's caller named, in order, as `<hook>=<player>`.
    fn callers(&self) -> Vec<String> {
        let callers = self.hooks.callers.lock().unwrap();
        callers
            .iter()
            .map(|(hook, caller)| {
                assert_eq!(
                    caller.as_object().unwrap().keys().filter(|key| *key != "player").collect::<Vec<_>>(),
                    ["kind"]
                );
                format!("{}={}", hook.rsplit('/').next().unwrap(), caller["player"].as_str().unwrap_or_default())
            })
            .collect()
    }

    async fn close(self) {
        self.platform.cleanup.close();
        self.platform.cleanup.wait().await;
        self.server.abort();
        let _ = self.server.await;
    }
}

fn claim(name: &str) -> Claim {
    Claim {
        operation_id: name.into(),
        connection_id: "connection".into(),
        player: PlayerIdentity { uuid: "alice".into(), username: "Alice".into(), ..Default::default() },
        ..Default::default()
    }
}

#[tokio::test]
async fn real_native_dispatch_orders_admission_rechecks_moves_and_pings_without_control() {
    let fixture = Fixture::new().await;
    assert!(String::from_utf8_lossy(&fixture.platform.status("localhost").await.unwrap()).contains("native"));
    assert_eq!(fixture.trace().await, "");
    let mut source = claim("login");
    source.demand = fixture.platform.route_claim(&source).await.unwrap();
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,");
    let destination = Claim {
        demand: SessionDemand {
            key: "arena".into(),
            session_type: "arena/default".into(),
            machine_profile: "local".into(),
        },
        ..claim("move")
    };
    fixture.platform.approve_move(&source, &destination).await.unwrap();
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,root,parent,arena,before,");
    fixture.hooks.backend.mutate("ban-operation".into(), Fixture::call("ban")).await.unwrap();
    assert_eq!(
        fixture.platform.approve_move(&source, &destination).await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,root,parent,arena,before,root,");
    // Hooks before the claim name no player; a move's name the player, whose source claim the gateway holds.
    assert_eq!(
        fixture.callers(),
        [
            "ping=",
            "root=",
            "route=",
            "parent=",
            "lobby=",
            "root=alice",
            "parent=alice",
            "arena=alice",
            "before=alice",
            "root=alice"
        ]
    );
    fixture.close().await;
}

#[tokio::test]
async fn a_platform_bound_to_another_deployment_runs_its_admission_rules() {
    let fixture = Fixture::new().await;
    fixture.hooks.backend.deploy(deployment("replacement", true)).await.unwrap();
    let mut source = claim("login");
    source.demand = fixture.platform.route_claim(&source).await.unwrap();
    let destination = Claim {
        demand: SessionDemand {
            key: "arena".into(),
            session_type: "arena/default".into(),
            machine_profile: "local".into(),
        },
        ..claim("move")
    };
    fixture.platform.approve_move(&source, &destination).await.unwrap();
    let replacement = fixture.platform.bind("replacement");
    assert_eq!(
        replacement.approve_move(&source, &destination).await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    fixture.close().await;
}

#[tokio::test]
async fn move_cancels_default_notifications_but_follow_player_retains_its_captured_scope() {
    let fixture = Fixture::new().await;
    let mut source = claim("login");
    source.demand = fixture.platform.route_claim(&source).await.unwrap();
    let mut lifecycle = Lifecycle::new(fixture.platform.clone());
    lifecycle.arrived(&source).unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let trace = fixture.trace().await;
            if trace.contains("enter-start") {
                assert!(!trace.contains("follow-start"), "{trace}");
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    lifecycle.cutover(&claim("move"));
    fixture.platform.cleanup.close();
    tokio::time::timeout(Duration::from_secs(3), fixture.platform.cleanup.wait()).await.unwrap();
    let trace = fixture.trace().await;
    assert!(trace.contains("follow-end"), "{trace}");
    assert!(!trace.contains("enter-end"), "{trace}");
    drop(lifecycle);
    fixture.close().await;
}

#[tokio::test]
async fn notification_execution_preserves_the_order_of_domain_transitions() {
    let fixture = Fixture::new().await;
    let mut source = claim("login");
    source.demand = fixture.platform.route_claim(&source).await.unwrap();
    let mut lifecycle = Lifecycle::new(fixture.platform.clone());
    lifecycle.arrived(&source).unwrap();
    fixture.platform.cleanup.close();
    tokio::time::timeout(Duration::from_secs(3), fixture.platform.cleanup.wait()).await.unwrap();
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,enter-start,enter-end,follow-start,follow-end,");
    assert_eq!(fixture.callers()[4..], ["enter=alice", "follow=alice"]);
    drop(lifecycle);
    fixture.close().await;
}

#[tokio::test]
async fn an_expired_notification_batch_cancels_its_running_hook() {
    let fixture = Fixture::new().await;
    let arena = Claim {
        demand: SessionDemand {
            key: "arena".into(),
            session_type: "arena/default".into(),
            machine_profile: "local".into(),
        },
        ..claim("login")
    };
    assert!(fixture.platform.manifest().await.unwrap().is_some());
    let mut lifecycle = Lifecycle::new(fixture.platform.clone());
    lifecycle.arrived(&arena).unwrap();
    fixture.platform.cleanup.close();
    tokio::time::timeout(Duration::from_secs(6), fixture.platform.cleanup.wait()).await.unwrap();
    // The batch's deadline passed a second before `stall` would have ended.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(fixture.trace().await, "enter-start,enter-end,hold-start,hold-end,stall-start,");
    drop(lifecycle);
    fixture.close().await;
}

fn deployment(id: &str, denies: bool) -> Deployment {
    let mut manifest:DomainManifest=serde_json::from_value(json!({"version":1,
        "scopes":{"":{"parent":null},"games":{"parent":""},"games/lobby":{"parent":"games"},"games/arena":{"parent":"games"}},
        "apps":{"lobby":"games/lobby","arena":"games/arena"},"hooks":{}})).unwrap();
    let mut source = String::from(
        r"
export function trace(ctx) { return ctx.db.get('state','trace')?.value ?? ''; }
export function record(ctx,value) { ctx.db.put('state','trace',{value:trace(ctx)+value+','}); return null; }
export function banned(ctx) { return ctx.db.get('state','ban')?.value === 'yes'; }
export function ban(ctx) { ctx.db.put('state','ban',{value:'yes'}); return null; }
",
    );
    for (index, (scope, event, name)) in [
        ("", HookEvent::PlayerLogin, "root"),
        ("", HookEvent::PlayerRoute, "route"),
        ("games", HookEvent::PlayerLogin, "parent"),
        ("games/lobby", HookEvent::PlayerLogin, "lobby"),
        ("games/arena", HookEvent::PlayerLogin, "arena"),
        ("games", HookEvent::PlayerBeforeMove, "before"),
        ("", HookEvent::ServerPing, "ping"),
        ("", HookEvent::PlayerConnect, "enter"),
        ("games/lobby", HookEvent::DomainEnter, "follow"),
        ("games/arena", HookEvent::PlayerConnect, "hold"),
        ("games/arena", HookEvent::DomainEnter, "stall"),
    ]
    .into_iter()
    .enumerate()
    {
        let export = format!("hook{index}");
        let prefix = if scope.is_empty() { "shared/domains".into() } else { format!("shared/domains/{scope}") };
        manifest.hooks.insert(
            format!("{prefix}/hooks/{name}"),
            chunk_contract::Hook {
                domain: scope.into(),
                event,
                export: export.clone(),
                order: None,
                follow_player: name == "follow",
            },
        );
        let body=match event {
            HookEvent::ServerPing=>"return {motd:'native',online:0,max:10};".into(),
            HookEvent::PlayerRoute=>"await ctx.runMutation('record','route'); return {key:'lobby',session_type:'lobby/default',machine_profile:'local'};".into(),
            HookEvent::PlayerConnect|HookEvent::DomainEnter=>{
                let sleep = if scope == "games/arena" { 3000 } else { 300 };
                format!("await ctx.runMutation('record','{name}-start'); await ctx.sleep({sleep}); await ctx.runMutation('record','{name}-end'); return null;")
            }
            _ if denies=>"return {allow:false,reason:'Closed'};".into(),
            _=>format!("await ctx.runMutation('record','{name}'); return {{allow:!(await ctx.runQuery('banned',null)),reason:'Banned'}};"),
        };
        write!(source, "\nexport async function {export}(ctx) {{ {body} }}").unwrap();
    }
    Deployment {
        contracts: Contracts { domains: Some(manifest), ..Default::default() },
        contract_version: chunk_contract::CONTRACT_VERSION,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: id.into(),
        source,
        tables: serde_json::from_value(json!({"state":{"fields":{"value":{"schema":{"type":"string"}}}}})).unwrap(),
        functions: [
            ("trace", FunctionKind::Query, Schema::Null, Schema::String),
            ("record", FunctionKind::Mutation, Schema::String, Schema::Null),
            ("banned", FunctionKind::Query, Schema::Null, Schema::Boolean),
            ("ban", FunctionKind::Mutation, Schema::Null, Schema::Null),
        ]
        .into_iter()
        .map(|(name, kind, arguments, result)| {
            (
                name.into(),
                Function {
                    kind,
                    export: name.into(),
                    visibility: if name == "record" || name == "banned" {
                        Visibility::Internal
                    } else {
                        Visibility::Public
                    },
                    arguments,
                    result,
                },
            )
        })
        .collect(),
    }
}
