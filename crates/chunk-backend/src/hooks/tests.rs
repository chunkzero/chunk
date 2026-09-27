use chunk_contract::{Contracts, DomainManifest, RuntimeProfile};
use chunk_js::DeploymentId;
use chunk_proto::v1::backend_hooks_server::BackendHooks;
use chunk_store::SqliteStore;
use serde_json::json;

use super::*;

const APPLICATION: &str = "application-credential-is-not-platform";
const PLATFORM: &str = "platform-credential-never-given-to-jvm";

fn deployment() -> Deployment {
    let domains: DomainManifest = serde_json::from_value(json!({
        "version":1,"scopes":{"":{"parent":null}},"apps":{},"hooks": {
            "shared/domains/hooks/login":{"domain":"","event":"player.login","export":"login"},
            "shared/domains/hooks/ping":{"domain":"","event":"server.ping","export":"ping"},
            "shared/domains/hooks/wait":{"domain":"","event":"player.connect","export":"wait"}
        }
    }))
    .unwrap();
    Deployment {
        contracts: Contracts { domains: Some(domains), ..Default::default() },
        contract_version:2,runtime_profile:RuntimeProfile::TransactionalV1,id:"hooks".into(),
        source:r"
export function read(ctx) { return ctx.db.get('counts','value')?.value ?? 0; }
export function increment(ctx) { const value=read(ctx)+1; ctx.db.put('counts','value',{value}); return value; }
export async function login(ctx) { const gateway=ctx.caller.kind==='gateway'&&Object.keys(ctx.caller).length===1; return {allow:gateway&&(await ctx.runQuery('read',null))===0}; }
export async function ping(ctx) { await ctx.runMutation('increment',null); return {motd:'invalid',online:0,max:10}; }
export async function wait(ctx) { await ctx.runMutation('increment',null); await ctx.sleep(10000); await ctx.runMutation('increment',null); return null; }
".into(),
        tables:serde_json::from_value(json!({"counts":{"fields":{"value":{"schema":{"type":"integer"}}}}})).unwrap(),
        functions:[("read",FunctionKind::Query),("increment",FunctionKind::Mutation)].into_iter().map(|(name,kind)|
            (name.into(),Function {kind,visibility:Visibility::Internal,export:name.into(),arguments:Schema::Null,result:Schema::Integer})).collect(),
    }
}

fn invocation(hook: &str) -> InvokeHook {
    let arguments = if hook == "ping" {
        json!({"domain":"","eventId":"ping","host":"localhost"})
    } else {
        json!({"domain":"","eventId":"login","destination":null,"player":{"uuid":"alice","username":"Alice"}})
    };
    InvokeHook {
        hook: format!("shared/domains/hooks/{hook}"),
        arguments_json: serde_json::to_vec(&arguments).unwrap(),
        caller_json: serde_json::to_vec(&json!({"kind":"proxy","proxyId":"proxy"})).unwrap(),
    }
}

fn request<T>(message: T, token: &str, deployment: &str) -> Request<T> {
    let mut request = Request::new(message);
    for (name, value) in [
        ("authorization", format!("Bearer {token}")),
        ("x-chunk-environment", "test".into()),
        ("x-chunk-deployment", deployment.into()),
    ] {
        request.metadata_mut().insert(name, value.parse().unwrap());
    }
    request
}

#[tokio::test]
async fn only_platform_authority_can_invoke_named_hooks_and_ping_is_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("test".into(), Box::new(SqliteStore::open(directory.path().join("hooks.db"), "test").unwrap()))
            .unwrap();
    backend.deploy(deployment()).await.unwrap();
    let service = HookService::new(backend.clone(), APPLICATION, PLATFORM).unwrap();
    assert!(!service.manifest(request((), APPLICATION, "hooks")).await.unwrap().into_inner().manifest_json.is_empty());
    assert_eq!(
        service.invoke(request(invocation("login"), APPLICATION, "hooks")).await.unwrap_err().code(),
        tonic::Code::Unauthenticated
    );
    assert_eq!(
        service.invoke(request(invocation("login"), PLATFORM, "missing")).await.unwrap_err().code(),
        tonic::Code::NotFound
    );
    let first = service.invoke(request(invocation("login"), PLATFORM, "hooks")).await.unwrap().into_inner();
    assert_eq!(serde_json::from_slice::<Value>(&first.result_json).unwrap(), json!({"allow":true}));
    let error = service.invoke(request(invocation("ping"), PLATFORM, "hooks")).await.unwrap_err();
    assert!(error.message().contains("read-only"), "{error}");
    let message = invocation("login");
    let call = Service::decode(
        DeploymentId::new("hooks").unwrap(),
        message.hook,
        &message.arguments_json,
        &message.caller_json,
    )
    .unwrap();
    assert!(matches!(
        backend.start_action(backend.allocate_action_id().await.unwrap(), call).await,
        Err(Error::Unknown)
    ));
    let mut arbitrary = invocation("login");
    arbitrary.hook = "increment".into();
    assert!(service.invoke(request(arbitrary, PLATFORM, "hooks")).await.is_err());
}

#[tokio::test]
async fn canceled_and_expired_hooks_cannot_resume_and_later_admission_reads_fresh_state() {
    let directory = tempfile::tempdir().unwrap();
    let backend =
        Backend::new("test".into(), Box::new(SqliteStore::open(directory.path().join("hooks.db"), "test").unwrap()))
            .unwrap();
    let mut deployment = deployment();
    deployment.functions.get_mut("read").unwrap().visibility = Visibility::Public;
    backend.deploy(deployment).await.unwrap();
    let service = HookService::new(backend.clone(), APPLICATION, PLATFORM).unwrap();
    let read = Call {
        deployment: DeploymentId::new("hooks").unwrap(),
        function: "read".into(),
        arguments: Value::Null.into(),
        caller: Value::Null.into(),
    };
    let mut updates = backend.subscribe(read.clone()).await.unwrap();
    updates.next().await.unwrap();
    let work = tokio::spawn({
        let service = service.clone();
        async move { service.invoke(request(invocation("wait"), PLATFORM, "hooks")).await }
    });
    tokio::time::timeout(Duration::from_secs(2), updates.next()).await.unwrap().unwrap();
    work.abort();
    let _ = work.await;
    let second = service.invoke(request(invocation("login"), PLATFORM, "hooks")).await.unwrap().into_inner();
    assert_eq!(serde_json::from_slice::<Value>(&second.result_json).unwrap(), json!({"allow":false}));
    let error = service.invoke(request(invocation("wait"), PLATFORM, "hooks")).await.unwrap_err();
    assert!(matches!(error.code(), tonic::Code::DeadlineExceeded | tonic::Code::Cancelled), "{error}");
    assert_eq!(&*backend.query(read).await.unwrap().json, "2");
}
