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

fn invocation(body: &str) -> Invocation {
    Invocation {
        deployment: "build-a".into(),
        source: format!("export default async (ctx, args) => {{ {body} }}"),
        export: "default".into(),
        arguments: json!({"id":"player"}),
        caller: json!({"player":"player"}),
        mode: Mode::Mutation,
    }
}
fn run(body: &str) -> Result<Execution, Error> {
    execute(
        invocation(body),
        Box::new(Snapshot),
        Limits::default(),
        &Cancellation::default(),
    )
}

#[test]
fn real_engine_reads_and_buffers_writes_with_fresh_globals_each_invocation() {
    let body = "globalThis.counter = (globalThis.counter || 0) + 1; const p = await ctx.db.get('profiles',args.id); ctx.db.put('profiles',args.id,{coins:p.coins+1}); return {counter, p:ctx.db.get('profiles',args.id),caller:ctx.caller.player};";
    for _ in 0..2 {
        let result = run(body).unwrap();
        assert_eq!(result.value, json!({"counter":1,"p":{"coins":4},"caller":"player"}));
        assert_eq!(result.writes.len(), 1);
        assert_eq!(result.writes[0].value, Some(json!({"coins":4})));
    }
    let mut other = invocation("return typeof counter;");
    other.deployment = "build-b".into();
    assert_eq!(
        execute(other, Box::new(Snapshot), Limits::default(), &Cancellation::default())
            .unwrap()
            .value,
        json!("undefined")
    );
    assert!(run("ctx.db.put('profiles','p',{}); throw Error('rollback');").is_err());
}

#[test]
fn ambient_apis_and_query_writes_are_denied() {
    let result = run("return [typeof Deno, typeof __bootstrap, typeof __infra, typeof fetch, typeof process, typeof Date, typeof Intl, typeof ArrayBuffer, typeof Uint8Array, typeof WebAssembly];").unwrap();
    assert_eq!(result.value, json!(vec!["undefined"; 10]));
    assert!(run("return Math.random();").is_err());
    assert!(run("return await import('ext:core/mod.js');").is_err());
    assert!(run("return await import('file:///etc/passwd');").is_err());
    assert!(run("return NaN;").is_err());
    let mut query = invocation("ctx.db.put('profiles','p',{}); return null;");
    query.mode = Mode::Query;
    assert!(execute(query, Box::new(Snapshot), Limits::default(), &Cancellation::default()).is_err());
}

#[test]
fn synchronous_loops_pending_promises_and_heap_exhaustion_are_bounded() {
    for body in [
        "while(true) {}",
        "await new Promise(() => {});",
        "while(true) { await Promise.resolve(); }",
    ] {
        let result = execute(
            invocation(body),
            Box::new(Snapshot),
            Limits {
                execution: Duration::from_millis(100),
                ..Limits::default()
            },
            &Cancellation::default(),
        );
        assert!(result.is_err(), "{body}");
    }
    let result = execute(
        invocation("const a=[]; while(true) a.push(new Array(10000).fill('xxxxxxxx'))"),
        Box::new(Snapshot),
        Limits {
            execution: Duration::from_secs(5),
            heap_bytes: 8 * 1024 * 1024,
        },
        &Cancellation::default(),
    );
    assert!(matches!(result, Err(Error::Heap)), "{result:?}");
    assert_eq!(run("return 42;").unwrap().value, json!(42));
}

#[test]
fn cancellation_interrupts_execution_and_discards_speculative_writes() {
    let cancellation = Cancellation::default();
    let trigger = cancellation.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        trigger.cancel();
    });
    let result = execute(
        invocation("ctx.db.put('profiles','p',{}); while(true) {}"),
        Box::new(Snapshot),
        Limits::default(),
        &cancellation,
    );
    thread.join().unwrap();
    assert!(matches!(result, Err(Error::Cancelled)), "{result:?}");
}
