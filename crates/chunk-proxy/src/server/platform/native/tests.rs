use std::{fmt::Write, time::Duration};

use crate::PlatformTarget;
use chunk_backend::{Backend, Call, HookService};
use chunk_contract::{Deployment, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::DeploymentId;
use chunk_proto::v1::{ClaimIdentity, Identity};
use chunk_store::SqliteStore;
use tokio_stream::wrappers::TcpListenerStream;

use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    backend: Backend,
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
        backend.deploy(deployment()).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let platform = Platform::new(PlatformTarget {
            backend: chunk_contract::BackendConnection {
                endpoint: format!("http://{}", listener.local_addr().unwrap()),
                environment: "test".into(),
                deployment: "candidate".into(),
                token: "application-only-token-for-tests".into(),
                platform_token: Some("trusted-platform-token-for-tests".into()),
            },
            control: chunk_contract::ControlConnection {
                endpoint: "http://127.0.0.1:1".into(),
                token: "unused".into(),
            },
        })
        .unwrap();
        let service = HookService::new(
            backend.clone(),
            &platform.target.backend.token,
            platform.target.backend.platform_token.as_ref().unwrap(),
        )
        .unwrap();
        let server = tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(service.into_server())
                .serve_with_incoming(TcpListenerStream::new(listener)),
        );
        Self { _directory: directory, backend, platform, server }
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
        serde_json::from_str(&self.backend.query(Self::call("trace")).await.unwrap().json).unwrap()
    }

    async fn close(self) {
        self.platform.cleanup.close();
        self.platform.cleanup.wait().await;
        self.server.abort();
        let _ = self.server.await;
    }
}

fn claim(name: &str) -> ClaimRequest {
    ClaimRequest {
        operation_id: name.into(),
        proxy_id: "proxy".into(),
        connection_id: "connection".into(),
        identity: Some(Identity { uuid: "alice".into(), username: "Alice".into(), ..Default::default() }),
        ..Default::default()
    }
}

#[tokio::test]
async fn real_native_dispatch_orders_admission_rechecks_moves_and_pings_without_control() {
    let fixture = Fixture::new().await;
    assert!(String::from_utf8_lossy(&fixture.platform.status("localhost").await.unwrap()).contains("native"));
    assert_eq!(fixture.trace().await, "");
    let mut source = claim("login");
    source.demand = Some(fixture.platform.route_claim(&source).await.unwrap());
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,");
    let destination = ClaimRequest {
        demand: Some(SessionDemand {
            key: "arena".into(),
            session_type: "arena/default".into(),
            machine_profile: "local".into(),
        }),
        ..claim("move")
    };
    fixture.platform.approve_move(&source, &destination).await.unwrap();
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,root,parent,arena,before,");
    fixture.backend.mutate("ban-operation".into(), Fixture::call("ban")).await.unwrap();
    assert_eq!(
        fixture.platform.approve_move(&source, &destination).await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(fixture.trace().await, "root,route,parent,lobby,root,parent,arena,before,root,");
    let mut missing = fixture.platform.target.clone();
    missing.backend.platform_token = None;
    assert!(
        Platform::new(missing)
            .unwrap()
            .route_claim(&claim("forged"))
            .await
            .unwrap_err()
            .to_string()
            .contains("authority")
    );
    let mut wrong = fixture.platform.target.clone();
    wrong.backend.platform_token = Some(wrong.backend.token.clone());
    assert!(Platform::new(wrong).unwrap().route_claim(&claim("forged")).await.is_err());
    fixture.close().await;
}

#[tokio::test]
async fn move_cancels_default_notifications_but_follow_player_retains_its_captured_scope() {
    let fixture = Fixture::new().await;
    let mut source = claim("login");
    source.demand = Some(fixture.platform.route_claim(&source).await.unwrap());
    let mut lifecycle = Lifecycle::new(fixture.platform.clone());
    lifecycle
        .arrived(
            &source,
            &ClaimIdentity {
                operation_id: "login".into(),
                proxy_id: "proxy".into(),
                membership_generation: 1,
                delivery_generation: 1,
            },
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let trace = fixture.trace().await;
            if trace.contains("follow-start") && trace.contains("enter-start") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    lifecycle.cutover(&claim("move"));
    tokio::time::sleep(Duration::from_millis(450)).await;
    let trace = fixture.trace().await;
    assert!(trace.contains("follow-end"), "{trace}");
    assert!(!trace.contains("enter-end"), "{trace}");
    drop(lifecycle);
    fixture.close().await;
}

fn deployment() -> Deployment {
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
        ("", HookEvent::PlayerConnect, "follow"),
        ("games/lobby", HookEvent::DomainEnter, "enter"),
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
            HookEvent::PlayerConnect|HookEvent::DomainEnter=>format!("await ctx.runMutation('record','{name}-start'); await ctx.sleep(300); await ctx.runMutation('record','{name}-end'); return null;"),
            _=>format!("await ctx.runMutation('record','{name}'); return {{allow:!(await ctx.runQuery('banned',null)),reason:'Banned'}};"),
        };
        write!(source, "\nexport async function {export}(ctx) {{ {body} }}").unwrap();
    }
    Deployment {
        contract_version: 2,
        runtime_profile: RuntimeProfile::TransactionalV1,
        id: "candidate".into(),
        source,
        domains: Some(manifest),
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
