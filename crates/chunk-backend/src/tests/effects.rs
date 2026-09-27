use std::{collections::BTreeMap, time::Duration};

use chunk_contract::{Contracts, Deployment, Field, Function, FunctionKind, RuntimeProfile, Schema, Visibility};
use chunk_js::DeploymentId;
use chunk_store::SqliteStore;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{ActionEffects, ActionGrants, Backend, Call, HttpBinding, HttpMethod};

struct Fixture {
    origin: String,
    requests: mpsc::UnboundedReceiver<String>,
    stop: CancellationToken,
    tasks: TaskTracker,
}
impl Fixture {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (observed, requests) = mpsc::unbounded_channel();
        let stop = CancellationToken::new();
        let tasks = TaskTracker::new();
        let worker_stop = stop.clone();
        let workers = tasks.clone();
        tasks.spawn(async move {
            loop {
                let accepted =
                    tokio::select! { ()=worker_stop.cancelled()=>break, accepted=listener.accept()=>accepted };
                let (mut stream, _) = accepted.unwrap();
                let stop = worker_stop.clone();
                let observed = observed.clone();
                workers.spawn(async move {
                    let exchange = async {
                        let mut request = Vec::new();
                        let mut buffer = [0; 4096];
                        loop {
                            let size = stream.read(&mut buffer).await.ok()?;
                            if size == 0 {
                                return None;
                            }
                            request.extend_from_slice(&buffer[..size]);
                            if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                break;
                            }
                            if request.len() > 100_000 {
                                return None;
                            }
                        }
                        let request = String::from_utf8(request).ok()?;
                        let path = request.split_whitespace().nth(1).unwrap_or_default().to_owned();
                        observed.send(request).ok()?;
                        let response = if path == "/api/redirect" {
                            "HTTP/1.1 302 Found\r\nLocation: /outside\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                .into()
                        } else if path == "/api/large" {
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                128 * 1024 + 1
                            )
                        } else if path == "/api/streamLarge" {
                            format!("HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}", "x".repeat(128 * 1024 + 1))
                        } else if path == "/api/drop" {
                            return None;
                        } else {
                            if path == "/api/slow" {
                                tokio::time::sleep(Duration::from_secs(2)).await;
                            }
                            "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone".into()
                        };
                        stream.write_all(response.as_bytes()).await.ok()
                    };
                    tokio::select! { ()=stop.cancelled()=>{}, _=exchange=>{} }
                });
            }
        });
        Self { origin, requests, stop, tasks }
    }

    async fn observed(&mut self) -> String {
        tokio::time::timeout(Duration::from_secs(3), self.requests.recv()).await.unwrap().unwrap()
    }

    async fn close(self) {
        self.stop.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn deployment(id: &str) -> Deployment {
    let fields: BTreeMap<_, _> = ["path", "binding", "body", "method"]
        .into_iter()
        .map(|name| (name.into(), Field { schema: Schema::String, optional: name == "body" || name == "method" }))
        .collect();
    Deployment {
        contracts: Contracts::default(),
        contract_version:2,runtime_profile:RuntimeProfile::TransactionalV1,id:id.into(),tables:BTreeMap::new(),
        source:r"
export async function run(ctx,args) { return JSON.stringify(await ctx.http(args.binding,{path:args.path,method:args.method??'GET',body:args.body})); }
export async function secret(ctx) { return String((await ctx.secret('token')).length); }
export async function credential(ctx) { return JSON.stringify(await ctx.http('api',{path:'ok',headers:{authorization:await ctx.secret('token')}})); }
export async function fanout(ctx) { return JSON.stringify(await Promise.all(Array.from({length:8},()=>ctx.http('api',{path:'slow'})))); }
export async function leak(ctx) { throw Error(await ctx.secret('token')); }
export function pure(ctx) { return typeof ctx.http+':'+typeof ctx.secret+':'+typeof fetch; }
".into(),
        functions:["run","secret","credential","fanout","leak","pure"].into_iter().map(|name| (name.into(),Function {
            kind:if name=="pure" {FunctionKind::Query} else {FunctionKind::Action},visibility:Visibility::Public,export:name.into(),
            arguments:if name=="run" {Schema::Object { fields:fields.clone() }} else {Schema::Null},result:Schema::String,
        })).collect(),
    }
}
fn call(deployment: &str, function: &str, args: Value) -> Call {
    Call {
        deployment: DeploymentId::new(deployment).unwrap(),
        function: function.into(),
        arguments: args.into(),
        caller: json!({"player":"alice"}).into(),
    }
}
fn configured(directory: &tempfile::TempDir, origin: &str, timeout: Duration) -> Backend {
    let grants = ActionGrants::default()
        .with_http(
            "api".into(),
            HttpBinding::new(&format!("{origin}/api/"), [HttpMethod::Get, HttpMethod::Post])
                .unwrap()
                .with_timeout(timeout)
                .unwrap(),
        )
        .unwrap()
        .with_secret("token".into(), "fixture-secret".into())
        .unwrap();
    let effects = ActionEffects::new("test".into())
        .unwrap()
        .with_deployment(DeploymentId::new("allowed").unwrap(), grants)
        .unwrap();
    Backend::with_action_bytes(
        "test".into(),
        Box::new(SqliteStore::open(directory.path().join("effects.db"), "test").unwrap()),
        effects,
        crate::limits::ACTION_BYTES,
    )
    .unwrap()
}
async fn outcome(backend: &Backend, deployment: &str, path: &str) -> Value {
    let mut action = backend
        .start_action(
            backend.allocate_action_id().await.unwrap(),
            call(deployment, "run", json!({"binding":"api","path":path})),
        )
        .await
        .unwrap();
    let result: String = serde_json::from_str(&action.outcome().await.unwrap()).unwrap();
    serde_json::from_str(&result).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_grants_default_deny_and_bind_environment_deployment_and_origin() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, &fixture.origin, Duration::from_secs(1));
    backend.deploy(deployment("allowed")).await.unwrap();
    backend.deploy(deployment("other")).await.unwrap();
    assert_eq!(outcome(&backend, "other", "ok").await["state"], "rejected");
    for path in [
        "/outside",
        "//other.invalid/",
        "../outside",
        "%2e%2e/outside",
        "%252e%252e/outside",
        "..;/outside",
        "%3b/outside",
        "nested/%2Foutside",
        "https://other.invalid/",
        "ok#fragment",
        "\\other.invalid",
    ] {
        assert_eq!(outcome(&backend, "allowed", path).await["state"], "rejected", "{path}");
    }
    let completed = outcome(&backend, "allowed", "ok").await;
    assert_eq!(completed["state"], "completed");
    assert_eq!(completed["body"], "done");
    assert!(completed["effectId"].as_str().unwrap().ends_with("/http/1"));
    assert!(fixture.observed().await.starts_with("GET /api/ok "));
    assert_eq!(outcome(&backend, "allowed", "redirect").await["status"], 302);
    assert!(fixture.observed().await.contains("/api/redirect"));
    assert!(fixture.requests.try_recv().is_err());
    assert_eq!(
        &*backend.query(call("allowed", "pure", json!(null))).await.unwrap().json,
        "\"undefined:undefined:undefined\""
    );
    for version in ["allowed", "other"] {
        let mut action = backend
            .start_action(backend.allocate_action_id().await.unwrap(), call(version, "secret", json!(null)))
            .await
            .unwrap();
        if version == "allowed" {
            assert_eq!(&*action.outcome().await.unwrap(), "\"14\"");
        } else {
            assert!(action.outcome().await.unwrap_err().to_string().contains("denied"));
        }
    }
    let mut credential = backend
        .start_action(backend.allocate_action_id().await.unwrap(), call("allowed", "credential", json!(null)))
        .await
        .unwrap();
    credential.outcome().await.unwrap();
    assert!(fixture.observed().await.contains("authorization: fixture-secret"));
    let mut leak = backend
        .start_action(backend.allocate_action_id().await.unwrap(), call("allowed", "leak", json!(null)))
        .await
        .unwrap();
    let error = leak.outcome().await.unwrap_err().to_string();
    assert!(error.contains("redacted") && !error.contains("fixture-secret"));
    let effects = ActionEffects::new("wrong".into()).unwrap();
    let store = SqliteStore::open(directory.path().join("wrong.db"), "test").unwrap();
    assert!(Backend::with_action_effects("test".into(), Box::new(store), effects).is_err());
    drop(backend);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_bounds_and_partial_failures_report_unknown_without_retries() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, &fixture.origin, Duration::from_millis(80));
    backend.deploy(deployment("allowed")).await.unwrap();
    for path in ["large", "streamLarge", "drop", "slow"] {
        let result = outcome(&backend, "allowed", path).await;
        assert_eq!(result["state"], "unknown", "{result}");
        assert!(fixture.observed().await.contains(&format!("/api/{path}")));
        assert!(fixture.requests.try_recv().is_err());
    }
    let mut oversized = backend
        .start_action(
            backend.allocate_action_id().await.unwrap(),
            call("allowed", "run", json!({"binding":"api","path":"ok","body":"x".repeat(64*1024+1),"method":"POST"})),
        )
        .await
        .unwrap();
    let result: String = serde_json::from_str(&oversized.outcome().await.unwrap()).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&result).unwrap()["state"], "rejected");
    assert!(fixture.requests.try_recv().is_err());
    drop(backend);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_after_server_observes_request_leaves_effect_uncertain_and_foreground_free() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, &fixture.origin, Duration::from_secs(5));
    backend.deploy(deployment("allowed")).await.unwrap();
    let mut action = backend
        .start_action(
            backend.allocate_action_id().await.unwrap(),
            call("allowed", "run", json!({"binding":"api","path":"slow"})),
        )
        .await
        .unwrap();
    let id = action.id().clone();
    assert!(fixture.observed().await.contains("/api/slow"));
    tokio::time::timeout(Duration::from_millis(250), backend.query(call("allowed", "pure", json!(null))))
        .await
        .unwrap()
        .unwrap();
    action.cancel();
    assert!(tokio::time::timeout(Duration::from_secs(1), action.outcome()).await.unwrap().is_err());
    let mut duplicate =
        backend.start_action(id, call("allowed", "run", json!({"binding":"api","path":"slow"}))).await.unwrap();
    assert!(duplicate.outcome().await.is_err());
    assert!(fixture.requests.try_recv().is_err());
    let mut fanout = backend
        .start_action(backend.allocate_action_id().await.unwrap(), call("allowed", "fanout", json!(null)))
        .await
        .unwrap();
    for _ in 0..8 {
        assert!(fixture.observed().await.contains("/api/slow"));
    }
    let rejected = outcome(&backend, "allowed", "ok").await;
    assert_eq!(rejected["state"], "rejected");
    assert!(rejected["reason"].as_str().unwrap().contains("concurrency"));
    fanout.cancel();
    assert!(fanout.outcome().await.is_err());
    drop(backend);
    fixture.close().await;
}

struct Child(std::process::Child);
impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "subprocess fixture for abrupt backend termination"]
fn http_crash_worker() {
    let Some(path) = std::env::var_os("CHUNK_HTTP_CRASH_STATE") else {
        return;
    };
    let origin = std::env::var("CHUNK_HTTP_CRASH_ORIGIN").unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let grants = ActionGrants::default()
            .with_http("api".into(), HttpBinding::new(&format!("{origin}/api/"), [HttpMethod::Get]).unwrap())
            .unwrap();
        let effects = ActionEffects::new("test".into())
            .unwrap()
            .with_deployment(DeploymentId::new("allowed").unwrap(), grants)
            .unwrap();
        let directory = std::path::Path::new(&path);
        let backend = Backend::with_action_effects(
            "test".into(),
            Box::new(SqliteStore::open(directory.join("crash.db"), "test").unwrap()),
            effects,
        )
        .unwrap();
        backend.deploy(deployment("allowed")).await.unwrap();
        let id = backend.allocate_action_id().await.unwrap();
        std::fs::write(
            directory.join("invocation.json"),
            serde_json::to_vec(&json!({"incarnation":id.incarnation,"sequence":id.sequence})).unwrap(),
        )
        .unwrap();
        let mut action =
            backend.start_action(id, call("allowed", "run", json!({"binding":"api","path":"slow"}))).await.unwrap();
        let _ = action.outcome().await;
        std::future::pending::<()>().await;
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abrupt_backend_crash_never_replays_an_observed_http_effect() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "tests::effects::http_crash_worker", "--ignored", "--nocapture"])
        .env("CHUNK_HTTP_CRASH_STATE", directory.path())
        .env("CHUNK_HTTP_CRASH_ORIGIN", &fixture.origin)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut child = Child(child);
    assert!(fixture.observed().await.contains("/api/slow"));
    child.0.kill().unwrap();
    child.0.wait().unwrap();
    let saved: Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("invocation.json")).unwrap()).unwrap();
    let id = crate::ActionId {
        incarnation: saved["incarnation"].as_str().unwrap().into(),
        sequence: saved["sequence"].as_u64().unwrap(),
    };
    let backend =
        Backend::new("test".into(), Box::new(SqliteStore::open(directory.path().join("crash.db"), "test").unwrap()))
            .unwrap();
    assert!(matches!(
        backend.start_action(id, call("allowed", "run", json!({"binding":"api","path":"slow"}))).await,
        Err(crate::Error::ActionOutcomeUnknown)
    ));
    assert!(fixture.requests.try_recv().is_err());
    drop(backend);
    fixture.close().await;
}
