use std::sync::Arc;

use chunk_contract::{Contracts, DomainManifest, RuntimeProfile};
use chunk_js::DeploymentId;
use chunk_store::SqliteStore;
use serde_json::json;

use super::*;
use crate::{ActionHandle, Backend};

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
        contract_version:3,runtime_profile:RuntimeProfile::TransactionalV1,id:"hooks".into(),
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

/// A gateway's call of `hook`, which names no player.
fn call(hook: &str) -> Call {
    let arguments = if hook == "ping" {
        json!({"domain":"","eventId":"ping","host":"localhost"})
    } else {
        json!({"domain":"","eventId":"login","destination":null,"player":{"uuid":"alice","username":"Alice"}})
    };
    Call {
        deployment: DeploymentId::new("hooks").unwrap(),
        function: format!("shared/domains/hooks/{hook}"),
        arguments: arguments.into(),
        caller: json!({"kind":"gateway"}).into(),
    }
}

async fn start(backend: &Backend, call: Call) -> Result<ActionHandle> {
    backend.start_hook(backend.allocate_action_id().await?, call).await
}

async fn run(backend: &Backend, call: Call) -> Result<Arc<str>> {
    start(backend, call).await?.outcome().await
}

async fn backend(directory: &tempfile::TempDir, deployment: Deployment) -> Backend {
    let store = SqliteStore::open(directory.path().join("hooks.db"), "test").unwrap();
    let backend = Backend::new("test".into(), Box::new(store)).unwrap();
    backend.deploy(deployment).await.unwrap();
    backend
}

#[tokio::test]
async fn only_declared_hooks_run_as_hooks_and_ping_is_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory, deployment()).await;
    let first = run(&backend, call("login")).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&first).unwrap(), json!({"allow":true}));
    let error = run(&backend, call("ping")).await.unwrap_err();
    assert!(error.to_string().contains("read-only"), "{error}");
    assert!(matches!(
        backend.start_action(backend.allocate_action_id().await.unwrap(), call("login")).await,
        Err(Error::Unknown)
    ));
    let mut arbitrary = call("login");
    arbitrary.function = "increment".into();
    assert!(start(&backend, arbitrary).await.is_err());
}

#[tokio::test]
async fn canceled_and_expired_hooks_cannot_resume_and_later_admission_reads_fresh_state() {
    let directory = tempfile::tempdir().unwrap();
    let mut deployment = deployment();
    deployment.functions.get_mut("read").unwrap().visibility = Visibility::Public;
    let backend = backend(&directory, deployment).await;
    let read = Call {
        deployment: DeploymentId::new("hooks").unwrap(),
        function: "read".into(),
        arguments: Value::Null.into(),
        caller: Value::Null.into(),
    };
    let mut updates = backend.subscribe(read.clone()).await.unwrap();
    updates.next().await.unwrap();
    let waiting = start(&backend, call("wait")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), updates.next()).await.unwrap().unwrap();
    drop(waiting);
    let second = run(&backend, call("login")).await.unwrap();
    assert_eq!(serde_json::from_str::<Value>(&second).unwrap(), json!({"allow":false}));
    let expired = tokio::time::timeout(HOOK_TIMEOUT * 2, run(&backend, call("wait"))).await.unwrap();
    assert!(expired.is_err());
    assert_eq!(&*backend.query(read).await.unwrap().json, "2");
}
