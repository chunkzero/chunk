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
            result.value,
            json!({"counter":counter,"p":{"coins":4},"caller":"player"})
        );
        assert_eq!(result.writes.len(), 1);
        assert_eq!(result.writes[0].value, Some(json!({"coins":4})));
    }
    // Separate owners also isolate environments that happen to use the same deployment ID.
    for id in ["build-b", "build-a"] {
        let mut other = Deployment::new(id.into(), source.into(), Limits::default()).unwrap();
        assert_eq!(call(&mut other).unwrap().value["counter"], json!(1));
        assert!(call(&mut first).unwrap().value["counter"].as_u64().unwrap() > 2);
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
        result.value,
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
    assert_eq!(call(&mut engine).unwrap().value, json!(1));
    let mut fail = invocation();
    fail.arguments = json!({"fail":true});
    assert!(
        engine
            .execute(fail, Box::new(Snapshot), &Cancellation::default())
            .is_err()
    );
    let result = call(&mut engine).unwrap();
    assert_eq!(result.value, json!(1));
    assert!(result.writes.is_empty());
}

#[test]
fn ambient_apis_and_query_writes_are_denied() {
    let result = run("return [typeof Deno, typeof __bootstrap, typeof __infra, typeof fetch, typeof process, typeof Date, typeof Intl, typeof ArrayBuffer, typeof Uint8Array, typeof WebAssembly];").unwrap();
    assert_eq!(result.value, json!(vec!["undefined"; 10]));
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
        assert_eq!(call(&mut engine).unwrap().value, json!(42));
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
    assert_eq!(call(&mut engine).unwrap().value, json!(42));
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
    assert_eq!(result.value, json!(42));
    assert!(result.writes.is_empty());
}

#[test]
fn absent_return_is_null_and_missing_exports_are_invalid() {
    assert_eq!(run("").unwrap().value, Value::Null);
    assert!(run("return { nested: undefined };").is_err());
    let mut engine = deployment("return 42;", Limits::default());
    let mut input = invocation();
    input.export = "missing".into();
    assert!(matches!(
        engine.execute(input, Box::new(Snapshot), &Cancellation::default()),
        Err(Error::Invalid("missing export"))
    ));
}
