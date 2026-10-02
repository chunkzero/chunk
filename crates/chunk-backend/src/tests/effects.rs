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

use crate::{ActionEffects, ActionStatus, Backend, Call, limits::ACTION_BYTES};

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
                        let redirect = |location: &str| {
                            format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        };
                        let response = if path == "/api/redirect" {
                            redirect("/api/ok")
                        } else if path == "/api/choices" {
                            "HTTP/1.1 300 Multiple Choices\r\nLocation: /api/ok\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                        } else if path == "/api/metadata" {
                            redirect("http://169.254.169.254/latest/meta-data/")
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
    let fields: BTreeMap<_, _> = ["url", "body", "method"]
        .into_iter()
        .map(|name| (name.into(), Field { schema: Schema::String, optional: name != "url" }))
        .collect();
    let env = chunk_contract::EnvManifest {
        vars: [("GREETING".into(), "hello".into())].into(),
        environments: [("prod".into(), [("GREETING".into(), "welcome".into())].into())].into(),
        secrets: ["TOKEN".into()].into(),
    };
    Deployment {
        contracts: Contracts { env, ..Contracts::default() },
        contract_version: chunk_contract::CONTRACT_VERSION,runtime_profile:RuntimeProfile::TransactionalV1,id:id.into(),tables:BTreeMap::new(),
        source:r"
export async function run(ctx,args) { return JSON.stringify(await ctx.fetch({url:args.url,method:args.method??'GET',body:args.body})); }
export async function credential(ctx,args) { return JSON.stringify(await ctx.fetch({url:args.url,headers:{authorization:ctx.env.TOKEN}})); }
export async function fanout(ctx,args) { return JSON.stringify(await Promise.all(Array.from({length:8},()=>ctx.fetch({url:args.url})))); }
export async function leak(ctx) { throw Error(ctx.env.TOKEN); }
export async function held(ctx) { const token=ctx.env.TOKEN; await ctx.sleep(300); return String(token)+':'+String(ctx.env.TOKEN); }
export async function env(ctx) { return JSON.stringify(ctx.env); }
export function pure(ctx) { return typeof ctx.fetch+':'+JSON.stringify(ctx.env)+':'+typeof fetch; }
export function echo(ctx,args) { console.log('echoed', args.url); return 'echoed'; }
export async function logs(ctx) { console.log({body:JSON.stringify({token:ctx.env.TOKEN})}); return ctx.runQuery('echo',{url:ctx.env.TOKEN}); }
export function later(ctx,args) { return ctx.scheduler.runAt(Date.now()+200,'env',null); }
export async function late(ctx) { await ctx.sleep(500); console.log('logged late'); return 'late'; }
".into(),
        functions:["run","credential","fanout","leak","held","env","pure","echo","logs","later","late"].into_iter().map(|name| (name.into(),Function {
            kind:match name { "pure" | "echo" => FunctionKind::Query, "later" => FunctionKind::Mutation, _ => FunctionKind::Action },
            visibility:Visibility::Public,export:name.into(),
            arguments:if matches!(name, "run" | "credential" | "fanout" | "echo") {Schema::Object { fields:fields.clone() }} else {Schema::Null},result:Schema::String,
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
/// Admits the fixture's loopback address beside public ones.
fn fixture_policy(address: std::net::IpAddr) -> bool {
    address == std::net::Ipv4Addr::LOCALHOST || chunk_service::net::public(address)
}
fn secrets(token: &str) -> crate::Secrets {
    [("TOKEN".to_owned(), token.to_owned())].into_iter().collect()
}
fn configured(directory: &tempfile::TempDir, action_bytes: usize) -> Backend {
    let effects = ActionEffects::new("test".into()).unwrap().with_policy(fixture_policy);
    let store = SqliteStore::open(directory.path().join("effects.db"), "test").unwrap();
    let backend = Backend::with_action_bytes("test".into(), Box::new(store), effects, action_bytes).unwrap();
    backend.set_secrets(secrets("fixture-secret"));
    backend
}
async fn finish(backend: &Backend, call: Call) -> crate::Result<std::sync::Arc<str>> {
    backend.start_action(backend.allocate_action_id().await.unwrap(), call).await.unwrap().outcome().await
}
async fn outcome(backend: &Backend, url: &str) -> Value {
    let result: String =
        serde_json::from_str(&finish(backend, call("allowed", "run", json!({"url":url}))).await.unwrap()).unwrap();
    serde_json::from_str(&result).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fetch_refuses_private_addresses_after_resolution_and_on_redirects() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(directory.path().join("strict.db"), "test").unwrap();
    let strict = Backend::new("test".into(), Box::new(store)).unwrap();
    strict.deploy(deployment("allowed")).await.unwrap();
    let port = fixture.origin.rsplit(':').next().unwrap().to_owned();
    for url in [
        format!("{}/api/ok", fixture.origin),
        format!("http://localhost:{port}/api/ok"),
        format!("http://[::ffff:127.0.0.1]:{port}/api/ok"),
        "http://169.254.169.254/latest/meta-data/".into(),
        "http://[fd00:ec2::254]/".into(),
        "http://10.0.0.1/".into(),
    ] {
        let result = outcome(&strict, &url).await;
        assert_eq!(result["state"], "rejected", "{url}: {result}");
        assert_eq!(result["reason"], "HTTP destination address refused", "{url}");
    }
    assert!(fixture.requests.try_recv().is_err());
    drop(strict);

    let backend = configured(&directory, ACTION_BYTES);
    backend.deploy(deployment("allowed")).await.unwrap();
    let followed = outcome(&backend, &format!("{}/api/redirect", fixture.origin)).await;
    assert_eq!((followed["state"].as_str(), followed["body"].as_str()), (Some("completed"), Some("done")));
    assert_eq!(followed["url"], format!("{}/api/ok", fixture.origin));
    assert!(fixture.observed().await.contains("/api/redirect"));
    assert!(fixture.observed().await.contains("/api/ok"));
    let choices = outcome(&backend, &format!("{}/api/choices", fixture.origin)).await;
    assert_eq!((choices["state"].as_str(), choices["status"].as_u64()), (Some("completed"), Some(300)), "{choices}");
    assert!(fixture.observed().await.contains("/api/choices"));
    let refused = outcome(&backend, &format!("{}/api/metadata", fixture.origin)).await;
    assert_eq!(refused["state"], "unknown", "{refused}");
    assert!(refused["reason"].as_str().unwrap().contains("refused"));
    assert!(fixture.observed().await.contains("/api/metadata"));
    for url in ["ftp://example.com/", "http://user:pass@example.com/", "not a url"] {
        assert_eq!(outcome(&backend, url).await["state"], "rejected", "{url}");
    }
    drop(backend);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn env_holds_vars_everywhere_and_secrets_in_actions_only() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, ACTION_BYTES);
    backend.deploy(deployment("allowed")).await.unwrap();
    assert_eq!(
        &*backend.query(call("allowed", "pure", json!(null))).await.unwrap().json,
        r#""undefined:{\"GREETING\":\"hello\"}:undefined""#
    );
    let env = finish(&backend, call("allowed", "env", json!(null))).await.unwrap();
    assert_eq!(&*env, r#""{\"GREETING\":\"hello\",\"TOKEN\":\"fixture-secret\"}""#);
    let url = format!("{}/api/ok", fixture.origin);
    finish(&backend, call("allowed", "credential", json!({"url":url}))).await.unwrap();
    assert!(fixture.observed().await.contains("authorization: fixture-secret"));
    let error = finish(&backend, call("allowed", "leak", json!(null))).await.unwrap_err().to_string();
    assert!(error.contains("redacted") && !error.contains("fixture-secret"));
    let effects = ActionEffects::new("wrong".into()).unwrap();
    let store = SqliteStore::open(directory.path().join("wrong.db"), "test").unwrap();
    assert!(Backend::with_action_effects("test".into(), Box::new(store), effects).is_err());
    drop(backend);
    fixture.close().await;
}

/// What every test in this process has logged through `console`.
fn console_logs() -> String {
    static LOGS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Vec<u8>>>> = std::sync::OnceLock::new();
    let logs = LOGS.get_or_init(|| {
        let logs = std::sync::Arc::<std::sync::Mutex<Vec<u8>>>::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(move || Capture(writer.clone()))
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
        logs
    });
    String::from_utf8_lossy(&logs.lock().unwrap()).into_owned()
}
struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buffer);
        Ok(buffer.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn logs_redact_secrets_inside_nested_json_and_nested_calls() {
    console_logs();
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, ACTION_BYTES);
    backend.set_secrets(secrets("nested\nredaction-secret"));
    backend.deploy(deployment("allowed")).await.unwrap();
    assert_eq!(&*finish(&backend, call("allowed", "logs", json!(null))).await.unwrap(), r#""echoed""#);
    let logs = console_logs();
    assert!(logs.contains("function=\"echo\""), "{logs}");
    assert!(!logs.contains("redaction-secret"), "{logs}");
    drop(backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_logs_name_their_deployment_after_another_is_deployed() {
    console_logs();
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, ACTION_BYTES);
    backend.deploy(deployment("before")).await.unwrap();
    let id = backend.allocate_action_id().await.unwrap();
    let mut running = backend.start_action(id, call("before", "late", json!(null))).await.unwrap();
    backend.deploy(deployment("after")).await.unwrap();
    assert!(matches!(*running.status.borrow(), ActionStatus::Running));
    assert_eq!(&*running.outcome().await.unwrap(), r#""late""#);
    let logs = console_logs();
    let line = logs.lines().find(|line| line.contains("logged late")).unwrap();
    assert!(line.contains(r#"deployment="before""#), "{line}");
    drop(backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restored_jobs_read_the_secrets_installed_at_startup() {
    let directory = tempfile::tempdir().unwrap();
    let open = |secrets: crate::Secrets| {
        let effects = ActionEffects::new("test".into()).unwrap().with_secrets(secrets);
        let store = SqliteStore::open(directory.path().join("jobs.db"), "test").unwrap();
        Backend::with_action_bytes("test".into(), Box::new(store), effects, ACTION_BYTES).unwrap()
    };
    let first = open(crate::Secrets::default());
    first.deploy(deployment("allowed")).await.unwrap();
    let scheduled = first.mutate("later".into(), call("allowed", "later", json!(null))).await.unwrap();
    let id: String = serde_json::from_str(&scheduled.json).unwrap();
    drop(first);
    let restored = open(secrets("restored-secret"));
    let job = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let job = restored.job(id.clone(), json!({"player":"alice"}).into()).await.unwrap();
            if job.state == chunk_store::JobState::Succeeded {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(job.result.unwrap().to_string().contains("restored-secret"));
    drop(restored);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn running_actions_keep_their_secrets_while_new_ones_see_rotation() {
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, ACTION_BYTES);
    backend.deploy(deployment("allowed")).await.unwrap();
    let id = backend.allocate_action_id().await.unwrap();
    let mut running = backend.start_action(id, call("allowed", "held", json!(null))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    backend.set_secrets(secrets("rotated"));
    let env = finish(&backend, call("allowed", "env", json!(null))).await.unwrap();
    assert!(env.contains("rotated") && !env.contains("fixture-secret"));
    assert_eq!(&*running.outcome().await.unwrap(), "\"fixture-secret:fixture-secret\"");
    backend.set_secrets(crate::Secrets::default());
    let env = finish(&backend, call("allowed", "env", json!(null))).await.unwrap();
    assert_eq!(&*env, r#""{\"GREETING\":\"hello\"}""#);
    drop(backend);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_bounds_and_partial_failures_report_unknown_without_retries() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, ACTION_BYTES);
    backend.deploy(deployment("allowed")).await.unwrap();
    for path in ["large", "streamLarge", "drop"] {
        let result = outcome(&backend, &format!("{}/api/{path}", fixture.origin)).await;
        assert_eq!(result["state"], "unknown", "{result}");
        assert!(fixture.observed().await.contains(&format!("/api/{path}")));
        assert!(fixture.requests.try_recv().is_err());
    }
    let oversized = json!({"url":format!("{}/api/ok", fixture.origin),"body":"x".repeat(64*1024+1),"method":"POST"});
    let result: String =
        serde_json::from_str(&finish(&backend, call("allowed", "run", oversized)).await.unwrap()).unwrap();
    assert_eq!(serde_json::from_str::<Value>(&result).unwrap()["state"], "rejected");
    assert!(fixture.requests.try_recv().is_err());
    drop(backend);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_after_server_observes_request_leaves_effect_uncertain_and_foreground_free() {
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    let backend = configured(&directory, ACTION_BYTES);
    backend.deploy(deployment("allowed")).await.unwrap();
    let slow = json!({"url":format!("{}/api/slow", fixture.origin)});
    let mut action = backend
        .start_action(backend.allocate_action_id().await.unwrap(), call("allowed", "run", slow.clone()))
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
    let mut duplicate = backend.start_action(id, call("allowed", "run", slow.clone())).await.unwrap();
    assert!(duplicate.outcome().await.is_err());
    assert!(fixture.requests.try_recv().is_err());
    let mut fanout = backend
        .start_action(backend.allocate_action_id().await.unwrap(), call("allowed", "fanout", slow))
        .await
        .unwrap();
    for _ in 0..8 {
        assert!(fixture.observed().await.contains("/api/slow"));
    }
    let rejected = outcome(&backend, &format!("{}/api/ok", fixture.origin)).await;
    assert_eq!(rejected["state"], "rejected");
    assert!(rejected["reason"].as_str().unwrap().contains("concurrency"));
    fanout.cancel();
    assert!(fanout.outcome().await.is_err());
    drop(backend);
    fixture.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn larger_action_budgets_raise_live_action_http_and_job_limits() {
    use super::jobs::{now, schedule, state};
    let mut fixture = Fixture::start().await;
    let directory = tempfile::tempdir().unwrap();
    // Sixteen live actions, with an HTTP slot each and a quarter of them for jobs.
    let budget = 16 * chunk_js::Limits::default().heap_bytes;
    let backend = configured(&directory, budget);
    backend.deploy(deployment("allowed")).await.unwrap();
    backend.deploy(super::jobs::deployment("old", 1)).await.unwrap();
    let mut requests = Vec::new();
    for _ in 0..12 {
        let run = call("allowed", "run", json!({"url":format!("{}/api/slow", fixture.origin)}));
        requests.push(backend.start_action(backend.allocate_action_id().await.unwrap(), run).await.unwrap());
    }
    for _ in 0..12 {
        assert!(fixture.observed().await.contains("/api/slow"));
    }
    // The fixture holds each request for two seconds, so all twelve actions and requests are live at once.
    assert!(requests.iter().all(|action| matches!(action.status(), ActionStatus::Running)));
    for index in 0..4 {
        let id = schedule(&backend, &format!("job-{index}"), now(), 30_000).await;
        state(&backend, &id, chunk_store::JobState::Running).await;
    }
    for mut action in requests {
        let result: String = serde_json::from_str(&action.outcome().await.unwrap()).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&result).unwrap()["state"], "completed");
    }
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
        let effects = ActionEffects::new("test".into()).unwrap().with_policy(fixture_policy);
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
        let slow = json!({"url":format!("{origin}/api/slow")});
        let mut action = backend.start_action(id, call("allowed", "run", slow)).await.unwrap();
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
        backend.start_action(id, call("allowed", "run", json!({"url":format!("{}/api/slow", fixture.origin)}))).await,
        Err(crate::Error::ActionOutcomeUnknown)
    ));
    assert!(fixture.requests.try_recv().is_err());
    drop(backend);
    fixture.close().await;
}
