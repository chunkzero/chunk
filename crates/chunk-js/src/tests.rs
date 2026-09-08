use super::*;
use serde_json::{Value, json};
use std::time::Duration;

struct Snapshot;
impl ReadHost for Snapshot {
    fn get(&mut self, _: &Key) -> Result<Option<Value>, String> {
        Ok(Some(json!({"coins": 3})))
    }
    fn scan(&mut self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Vec<(String, Value)>, String> {
        Ok(vec![])
    }
}

fn invocation() -> Invocation {
    Invocation {
        export: "default".into(),
        arguments: json!({"id":"player"}),
        caller: json!({"player":"player"}),
        mode: Mode::Mutation,
        timestamp: 1_700_000_000_000,
        seed: 7,
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
    let result = run("return [typeof Deno, typeof __bootstrap, typeof __infra, typeof fetch, typeof process, typeof Intl, typeof SharedArrayBuffer, typeof WebAssembly];").unwrap();
    assert_eq!(result.value, json!(vec!["undefined"; 8]));
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
fn invocation_time_and_randomness_are_deterministic_without_removing_date_behavior() {
    let mut engine = Deployment::new(
        "clock".into(),
        r"
        const epoch = new Date(0);
        const bytes = new Uint8Array([1, 2, 3]);
        class GameDate extends Date {}
        export default () => ({
          now: Date.now(), constructed: +new Date(), called: Date(),
          inherited: +new GameDate(), epoch: +epoch,
          parsed: Date.parse('1970-01-01T00:00:00.000Z'),
          utc: Date.UTC(1970, 0, 1), instance: epoch instanceof Date,
          subclass: new GameDate() instanceof GameDate,
          restored: +new (epoch.constructor)(),
          bytes: Array.from(bytes), random: [Math.random(), Math.random()]
        });
        "
        .into(),
        Limits::default(),
    )
    .unwrap();
    let first = call(&mut engine).unwrap().value;
    assert_eq!(call(&mut engine).unwrap().value, first);
    for key in ["now", "constructed", "inherited", "restored"] {
        assert_eq!(first[key], json!(invocation().timestamp));
    }
    assert_eq!(first["epoch"], json!(0));
    assert_eq!(first["parsed"], json!(0));
    assert_eq!(first["utc"], json!(0));
    assert_eq!(first["instance"], json!(true));
    assert_eq!(first["subclass"], json!(true));
    assert_eq!(first["bytes"], json!([1, 2, 3]));
    let mut next = invocation();
    next.timestamp += 1;
    next.seed += 1;
    let second = engine
        .execute(next, Box::new(Snapshot), &Cancellation::default())
        .unwrap();
    assert_ne!(first["now"], second.value["now"]);
    assert_ne!(first["random"], second.value["random"]);
    for value in second.value["random"].as_array().unwrap() {
        assert!((0.0..1.0).contains(&value.as_f64().unwrap()));
    }
}

#[test]
fn initialization_cannot_observe_invocation_time_or_randomness() {
    for expression in [
        "Date.now()",
        "new Date()",
        "Date()",
        "Math.random()",
        "new Date(0).constructor.now()",
    ] {
        assert!(
            Deployment::new(
                "clock".into(),
                format!("const value = {expression}; export default () => value;"),
                Limits::default(),
            )
            .is_err(),
            "{expression}"
        );
    }
    let mut engine = deployment("return Date.now();", Limits::default());
    let mut invalid = invocation();
    invalid.timestamp = i64::MAX;
    assert!(matches!(
        engine.execute(invalid, Box::new(Snapshot), &Cancellation::default()),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn buffer_budget_bounds_total_retained_allocations_and_recycles_after_exhaustion() {
    let mut engine = deployment(
        "if (args.fail) { globalThis.buffers = []; for(let i=0; i<8; i++) buffers.push(new Uint8Array(2 * 1024 * 1024)); } return 42;",
        Limits {
            execution: Duration::from_secs(5),
            heap_bytes: 8 * 1024 * 1024,
        },
    );
    let mut fail = invocation();
    fail.arguments = json!({"fail": true});
    let result = engine.execute(fail, Box::new(Snapshot), &Cancellation::default());
    assert!(matches!(result, Err(Error::Heap)), "{result:?}");
    assert_eq!(call(&mut engine).unwrap().value, json!(42));
    assert!(run("return new ArrayBuffer(1, {maxByteLength: 1024}).resizable;").is_err());
    // Reading options once and omitting them from native construction prevents
    // a getter from sneaking a resizable allocation past the wrapper's check.
    assert_eq!(
        run("let reads=0; return new ArrayBuffer(1, {get maxByteLength() { return reads++ ? 1024 : undefined; }}).resizable;")
            .unwrap()
            .value,
        json!(false)
    );
    assert!(run("return new (new Uint8Array(1).buffer.constructor)(1, {maxByteLength: 1024}).resizable;").is_err());
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

#[test]
fn engine_merges_puts_and_deletes_into_raw_snapshot_ranges() {
    struct Rows;
    impl ReadHost for Rows {
        fn get(&mut self, _: &Key) -> Result<Option<Value>, String> {
            Ok(None)
        }
        fn scan(&mut self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Vec<(String, Value)>, String> {
            Ok(vec![("a".into(), json!(1)), ("b".into(), json!(2))])
        }
    }
    let mut engine = deployment(
        "ctx.db.delete('p','a'); ctx.db.put('p','b',3); ctx.db.put('p','c',4); ctx.db.put('p','z',5); return ctx.db.scan('p','a','d');",
        Limits::default(),
    );
    let result = engine
        .execute(invocation(), Box::new(Rows), &Cancellation::default())
        .unwrap();
    assert_eq!(result.value, json!([["b", 3], ["c", 4]]));
}
