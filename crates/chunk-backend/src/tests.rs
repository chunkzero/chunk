use crate::{Backend, Call, Error};
use chunk_contract::{Deployment, Function, FunctionKind, Schema};
use chunk_store::{Revision, SqliteStore};
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

const SOURCE: &str = r"
export function get(ctx) { return ctx.db.get('counters', 'count') ?? 0; }
export function increment(ctx) {
  const value = get(ctx) + 1;
  ctx.db.put('counters', 'count', value);
  return value;
}
export function claim(ctx, id) {
  if (ctx.db.scan('counters').length) return false;
  ctx.db.put('counters', id, 1);
  return true;
}
export function bad(ctx) { ctx.db.put('counters', 'count', 'invalid'); return 1; }
export function badResult(ctx) { ctx.db.put('counters', 'count', 9); return 'invalid'; }
export function throws(ctx) { ctx.db.put('counters', 'count', 9); throw Error('no'); }
";

fn deployment(id: &str) -> Deployment {
    let functions = [
        ("get", FunctionKind::Query, Schema::Null, Schema::Integer),
        ("increment", FunctionKind::Mutation, Schema::Null, Schema::Integer),
        ("claim", FunctionKind::Mutation, Schema::String, Schema::Boolean),
        ("bad", FunctionKind::Mutation, Schema::Null, Schema::Integer),
        ("badResult", FunctionKind::Mutation, Schema::Null, Schema::Integer),
        ("throws", FunctionKind::Mutation, Schema::Null, Schema::Integer),
    ]
    .into_iter()
    .map(|(name, kind, arguments, result)| {
        (
            name.into(),
            Function {
                export: name.into(),
                kind,
                arguments,
                result,
            },
        )
    })
    .collect();
    Deployment {
        id: id.into(),
        source: SOURCE.into(),
        tables: BTreeMap::from([("counters".into(), Schema::Integer)]),
        functions,
    }
}

fn backend(directory: &tempfile::TempDir) -> Backend {
    let backend = Backend::new(
        "local".into(),
        Box::new(SqliteStore::open(directory.path().join("data.db"), "local").unwrap()),
    )
    .unwrap();
    backend.register(deployment("a")).unwrap();
    backend
}

fn call(function: &str, operation: &str, arguments: Value) -> Call {
    Call {
        deployment: "a".into(),
        function: function.into(),
        arguments,
        caller: json!({"service": "test"}),
        operation: operation.into(),
    }
}

#[tokio::test]
async fn conflicting_points_and_phantom_inserts_reexecute_before_commit() {
    for phantom in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut backend = backend(&directory);
        backend.attempt_barrier = Some(Arc::new(tokio::sync::Barrier::new(2)));
        let function = if phantom { "claim" } else { "increment" };
        let args = |id| if phantom { json!(id) } else { Value::Null };
        let (first, second) = tokio::join!(
            backend.call(call(function, "one", args("one"))),
            backend.call(call(function, "two", args("two")))
        );
        let (first, second) = (first.unwrap(), second.unwrap());
        assert_ne!(first.revision, second.revision);
        if phantom {
            assert_ne!(first.result, second.result);
        } else {
            assert_eq!(first.result.as_u64().unwrap() + second.result.as_u64().unwrap(), 3);
            assert_eq!(
                backend.call(call("get", "", Value::Null)).await.unwrap().result,
                json!(2)
            );
        }
    }
}

#[tokio::test]
async fn failures_publish_nothing_and_restart_recovers_one_durable_outcome() {
    let directory = tempfile::tempdir().unwrap();
    let first = backend(&directory);
    for function in ["bad", "badResult", "throws"] {
        assert!(first.call(call(function, function, Value::Null)).await.is_err());
        assert_eq!(
            first.call(call("get", "", Value::Null)).await.unwrap().revision,
            Revision(1)
        );
    }
    assert!(matches!(
        first.call(call("increment", "invalid-args", json!(2))).await,
        Err(Error::Contract)
    ));
    let outcome = first.call(call("increment", "lost-reply", Value::Null)).await.unwrap();
    drop(first);
    let restarted = backend(&directory);
    assert_eq!(
        restarted
            .call(call("increment", "lost-reply", Value::Null))
            .await
            .unwrap(),
        outcome
    );
    assert!(matches!(
        restarted.call(call("bad", "lost-reply", Value::Null)).await,
        Err(Error::Store(chunk_store::Error::OperationMismatch))
    ));
    assert_eq!(
        restarted.call(call("get", "", Value::Null)).await.unwrap().result,
        json!(1)
    );
    let mut changed = deployment("a");
    changed.source.push_str("\n// different artifact");
    assert!(matches!(restarted.register(changed), Err(Error::Invalid(_))));
    drop(restarted);
    let retained = Backend::new(
        "local".into(),
        Box::new(SqliteStore::open(directory.path().join("data.db"), "local").unwrap()),
    )
    .unwrap();
    let mut newer = deployment("b");
    newer.tables.insert(
        "counters".into(),
        Schema::Union {
            variants: vec![Schema::Integer, Schema::String],
        },
    );
    retained.register(newer).unwrap();
    assert!(matches!(
        retained
            .call(Call {
                deployment: "b".into(),
                ..call("bad", "new-write", Value::Null)
            })
            .await,
        Err(Error::Contract)
    ));
}

#[tokio::test]
async fn service_rejects_wrong_credentials_and_environment_before_execution() {
    use chunk_proto::v1::{BackendCall, backend_server::Backend as _};
    use tonic::{Code, Request};
    let directory = tempfile::tempdir().unwrap();
    let service = crate::Service::new(backend(&directory), "test-credential-with-at-least-32-bytes").unwrap();
    let call = BackendCall {
        environment: "local".into(),
        deployment: "a".into(),
        function: "get".into(),
        arguments_json: b"null".to_vec(),
        caller_json: b"{}".to_vec(),
        operation_id: String::new(),
    };
    assert_eq!(
        service.call(Request::new(call.clone())).await.unwrap_err().code(),
        Code::Unauthenticated
    );
    let request = |call| {
        let mut request = Request::new(call);
        request.metadata_mut().insert(
            "authorization",
            "Bearer test-credential-with-at-least-32-bytes".parse().unwrap(),
        );
        request
    };
    assert_eq!(
        service
            .call(request(BackendCall {
                environment: "other".into(),
                ..call.clone()
            }))
            .await
            .unwrap_err()
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        service.call(request(call)).await.unwrap().into_inner().result_json,
        b"0"
    );
}

#[tokio::test]
async fn retained_versions_receive_atomic_groups_and_reconnect_to_fresh_data() {
    let directory = tempfile::tempdir().unwrap();
    let backend = backend(&directory);
    backend.register(deployment("b")).unwrap();
    let first = call("get", "", Value::Null);
    let second = Call {
        deployment: "b".into(),
        ..first.clone()
    };
    let queries = vec![first, second];
    let mut group = backend.subscribe(queries.clone()).unwrap();
    assert_eq!(group.next().await.unwrap().results, vec![json!(0), json!(0)]);
    backend.call(call("increment", "one", Value::Null)).await.unwrap();
    backend.call(call("increment", "two", Value::Null)).await.unwrap();
    let update = group.next().await.unwrap();
    assert_eq!(update.revision, Revision(4));
    assert_eq!(update.results, vec![json!(2), json!(2)]);
    drop(group);
    backend.call(call("increment", "three", Value::Null)).await.unwrap();
    let update = backend.subscribe(queries).unwrap().next().await.unwrap();
    assert_eq!(update.revision, Revision(5));
    assert_eq!(update.results, vec![json!(3), json!(3)]);
    let mut incompatible = deployment("c");
    incompatible.tables.insert("counters".into(), Schema::String);
    assert!(matches!(backend.register(incompatible), Err(Error::Contract)));
    let mut changed = deployment("a");
    changed.source.push_str("\n// changed");
    assert!(matches!(backend.register(changed), Err(Error::Invalid(_))));
}
