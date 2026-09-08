use super::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};

struct Snapshot;
impl ReadHost for Snapshot {
    fn read(&mut self, request: Read, overlay: &BTreeMap<Key, Option<Value>>) -> Result<Value, String> {
        match request {
            Read::Get { table, id } => Ok(overlay
                .get(&Key { table, id })
                .cloned()
                .unwrap_or(Some(json!({"coins": 3})))
                .unwrap_or(Value::Null)),
            Read::Scan { .. } => Ok(json!([])),
        }
    }
}

fn invocation() -> Invocation {
    Invocation {
        export: "default".into(),
        arguments: json!({"id":"player"}),
        caller: json!({"player":"player"}),
        mode: Mode::Mutation,
    }
}
fn deployment(body: &str, limits: Limits) -> Deployment {
    Deployment::new(
        "build-a".into(),
        format!("export default async (ctx, args) => {{ {body} }}"),
        limits,
    )
    .unwrap()
}
fn call(deployment: &mut Deployment) -> Result<Execution, Error> {
    deployment.execute(invocation(), Box::new(Snapshot), &Cancellation::default())
}
fn run(body: &str) -> Result<Execution, Error> {
    call(&mut deployment(body, Limits::default()))
}

#[test]
fn deployment_reuses_module_state_but_isolates_snapshots_and_other_deployments() {
    let source = "let counter = 0; export default async (ctx, args) => { counter++; const p = ctx.db.get('profiles',args.id); ctx.db.put('profiles',args.id,{coins:p.coins+1}); return {counter, p:ctx.db.get('profiles',args.id),caller:ctx.caller.player}; }";
    let mut first = Deployment::new("build-a".into(), source.into(), Limits::default()).unwrap();
    assert_eq!(first.id(), "build-a");
    for counter in 1..=2 {
        let result = call(&mut first).unwrap();
        assert_eq!(
            value(&result),
            json!({"counter":counter,"p":{"coins":4},"caller":"player"})
        );
        assert_eq!(result.writes.len(), 1);
        assert_eq!(result.writes[0].value, Some(json!({"coins":4})));
    }
    // Separate owners also isolate environments that happen to use the same deployment ID.
    for id in ["build-b", "build-a"] {
        let mut other = Deployment::new(id.into(), source.into(), Limits::default()).unwrap();
        assert_eq!(value(&call(&mut other).unwrap())["counter"], json!(1));
        assert!(value(&call(&mut first).unwrap())["counter"].as_u64().unwrap() > 2);
    }
}

#[test]
fn retained_capabilities_cannot_access_later_transactions_or_callers() {
    let mut engine = deployment(
        r"
        if (!globalThis.old) { globalThis.old = ctx; return null; }
        let denied = 0;
        for (const op of [() => old.db.get('profiles','p'), () => old.db.scan('profiles'),
            () => old.db.put('profiles','p',{}), () => old.db.delete('profiles','p')]) {
            try { op(); } catch { denied++; }
        }
        return {denied, old:old.caller.player, current:ctx.caller.player, p:ctx.db.get('profiles','p')};
    ",
        Limits::default(),
    );
    call(&mut engine).unwrap();
    let mut next = invocation();
    next.caller = json!({"player":"other"});
    let result = engine
        .execute(next, Box::new(Snapshot), &Cancellation::default())
        .unwrap();
    assert_eq!(
        value(&result),
        json!({"denied":4,"old":"player","current":"other","p":{"coins":3}})
    );
    assert!(result.writes.is_empty());
}

#[test]
fn failed_call_discards_writes_and_reloads_the_same_bundle() {
    let mut engine = deployment(
        "globalThis.count = (globalThis.count || 0) + 1; if (args.fail) { ctx.db.put('profiles','p',{}); throw Error('rollback'); } return count;",
        Limits::default(),
    );
    assert_eq!(value(&call(&mut engine).unwrap()), json!(1));
    let mut fail = invocation();
    fail.arguments = json!({"fail":true});
    assert!(
        engine
            .execute(fail, Box::new(Snapshot), &Cancellation::default())
            .is_err()
    );
    let result = call(&mut engine).unwrap();
    assert_eq!(value(&result), json!(1));
    assert!(result.writes.is_empty());
}

#[test]
fn ambient_apis_and_query_writes_are_denied() {
    let result = run("return [typeof Deno, typeof __bootstrap, typeof __infra, typeof fetch, typeof process, typeof Date, typeof Intl, typeof ArrayBuffer, typeof Uint8Array, typeof WebAssembly];").unwrap();
    assert_eq!(value(&result), json!(vec!["undefined"; 10]));
    assert!(run("return Math.random();").is_err());
    assert!(run("return await import('ext:core/mod.js');").is_err());
    assert!(run("return await import('file:///etc/passwd');").is_err());
    assert!(run("return NaN;").is_err());
    assert!(run("Promise.reject(Error('unhandled')); return 42;").is_err());
    let mut engine = deployment("ctx.db.put('profiles','p',{}); return null;", Limits::default());
    assert_eq!(call(&mut engine).unwrap().writes.len(), 1);
    let mut query = invocation();
    query.mode = Mode::Query;
    assert!(
        engine
            .execute(query, Box::new(Snapshot), &Cancellation::default())
            .is_err()
    );
}

#[test]
fn loops_pending_promises_and_heap_exhaustion_recycle_the_engine() {
    for body in [
        "while(true) {}",
        "await new Promise(() => {});",
        "while(true) { await Promise.resolve(); }",
    ] {
        let mut engine = deployment(
            &format!("if(args.fail) {{ {body} }} return 42;"),
            Limits {
                execution: Duration::from_millis(100),
                ..Limits::default()
            },
        );
        let mut fail = invocation();
        fail.arguments = json!({"fail":true});
        assert!(
            engine
                .execute(fail, Box::new(Snapshot), &Cancellation::default())
                .is_err(),
            "{body}"
        );
        assert_eq!(value(&call(&mut engine).unwrap()), json!(42));
    }
    let mut engine = deployment(
        "if(args.fail) { const a=[]; while(true) a.push(new Array(10000).fill('xxxxxxxx')); } return 42;",
        Limits {
            execution: Duration::from_secs(5),
            heap_bytes: 8 * 1024 * 1024,
        },
    );
    let mut fail = invocation();
    fail.arguments = json!({"fail":true});
    let result = engine.execute(fail, Box::new(Snapshot), &Cancellation::default());
    assert!(matches!(result, Err(Error::Heap)), "{result:?}");
    assert_eq!(value(&call(&mut engine).unwrap()), json!(42));
    assert!(
        Deployment::new(
            "bad".into(),
            "while(true) {}".into(),
            Limits {
                execution: Duration::from_millis(100),
                ..Limits::default()
            }
        )
        .is_err()
    );
}

#[test]
fn cancellation_interrupts_execution_and_discards_speculative_writes() {
    let mut engine = deployment(
        "if(args.fail) { ctx.db.put('profiles','p',{}); while(true) {} } return 42;",
        Limits::default(),
    );
    let cancellation = Cancellation::default();
    let trigger = cancellation.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        trigger.cancel();
    });
    let mut fail = invocation();
    fail.arguments = json!({"fail":true});
    let result = engine.execute(fail, Box::new(Snapshot), &cancellation);
    thread.join().unwrap();
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
    let result = call(&mut engine).unwrap();
    assert_eq!(value(&result), json!(42));
    assert!(result.writes.is_empty());
}

fn value(execution: &Execution) -> Value {
    serde_json::from_str(&execution.value).unwrap()
}

#[test]
fn json_boundary_preserves_unicode_and_rejects_non_json_results() {
    let mut engine = deployment("return {caller: ctx.caller, args};", Limits::default());
    let mut input = invocation();
    input.caller = json!({"name": "Alex 🦊"});
    input.arguments = json!({"雪": [null, true, "\\\"\n"]});
    let expected = json!({"caller": input.caller, "args": input.arguments});
    let result = engine
        .execute(input, Box::new(Snapshot), &Cancellation::default())
        .unwrap();
    assert_eq!(value(&result), expected);
    for expression in ["undefined", "Infinity", "1n", "({bad: undefined})", "[Symbol()]"] {
        assert!(run(&format!("return {expression};")).is_err(), "{expression}");
    }
    assert!(run("return 'x'.repeat(1024 * 1024);").is_err());
}

#[test]
fn engine_switches_releases_and_recycles_independent_deployments() {
    let mut engine = Engine::new().unwrap();
    let ids = ["z", "a", "m"].map(|id| DeploymentId::new(id).unwrap());
    let source = "let n=0; export default (_, args) => { if(args.fail) throw Error('fail'); return ++n; };";
    for id in &ids {
        engine.register(id.clone(), source.into(), Limits::default()).unwrap();
    }
    assert!(
        engine
            .register(ids[0].clone(), source.into(), Limits::default())
            .is_err()
    );
    let execute = |engine: &mut Engine, id: &DeploymentId, input| {
        engine.execute(id, input, Box::new(Snapshot), &Cancellation::default())
    };
    for expected in 1..=2 {
        for id in &ids {
            assert_eq!(value(&execute(&mut engine, id, invocation()).unwrap()), json!(expected));
        }
    }
    assert!(engine.release(&ids[0]));
    assert!(!engine.release(&ids[0]));
    assert!(matches!(
        execute(&mut engine, &ids[0], invocation()),
        Err(Error::UnknownDeployment)
    ));
    let bad = DeploymentId::new("bad").unwrap();
    assert!(
        engine
            .register(bad, "throw Error('init');".into(), Limits::default())
            .is_err()
    );
    let mut fail = invocation();
    fail.arguments = json!({"fail": true});
    assert!(execute(&mut engine, &ids[1], fail).is_err());
    assert_eq!(value(&execute(&mut engine, &ids[1], invocation()).unwrap()), json!(1));
    assert_eq!(value(&execute(&mut engine, &ids[2], invocation()).unwrap()), json!(3));
    for _ in 1..10_000 {
        execute(&mut engine, &ids[1], invocation()).unwrap();
    }
    assert_eq!(value(&execute(&mut engine, &ids[1], invocation()).unwrap()), json!(1));
    assert_eq!(value(&execute(&mut engine, &ids[2], invocation()).unwrap()), json!(4));
    // Remaining runtimes drop in map order, not reverse creation order.
}

#[test]
fn host_runs_on_the_callers_thread_without_send_or_locks() {
    use std::{cell::Cell, rc::Rc, thread};
    struct LocalHost {
        calls: Rc<Cell<usize>>,
        owner: thread::ThreadId,
    }
    impl ReadHost for LocalHost {
        fn read(&mut self, _: Read, _: &BTreeMap<Key, Option<Value>>) -> Result<Value, String> {
            assert_eq!(thread::current().id(), self.owner);
            self.calls.set(self.calls.get() + 1);
            Ok(json!(42))
        }
    }
    let mut engine = Engine::new().unwrap();
    let id = DeploymentId::new("local").unwrap();
    engine
        .register(
            id.clone(),
            "export default ctx => ctx.db.get('p','1');".into(),
            Limits::default(),
        )
        .unwrap();
    let calls = Rc::new(Cell::new(0));
    let host = LocalHost {
        calls: calls.clone(),
        owner: thread::current().id(),
    };
    let result = engine
        .execute(&id, invocation(), Box::new(host), &Cancellation::default())
        .unwrap();
    assert_eq!(value(&result), json!(42));
    assert_eq!(calls.get(), 1);
}
