use chunk_js::Limits;
use serde_json::{Value, json};

use super::{call, id, open};
use crate::{Backend, Call, GroupSubscription};

const SOURCE: &str = r"
let evaluations = 0;
export function board(ctx) {
  evaluations++;
  const coins = ctx.db.get('profiles', 'board')?.coins ?? 0;
  return coins > 1 ? ctx.caller.player : coins;
}
export function evaluated() { return evaluations; }
export function clock() { return Date.now(); }
export function put(ctx, args) { ctx.db.put('profiles', args.id, args.value); return null; }
";

fn player(name: &str) -> Call {
    let mut call = call("board", json!({}));
    call.caller = json!({ "player": name }).into();
    call
}

async fn evaluated(backend: &Backend) -> u64 {
    let update = backend.query(call("evaluated", json!({}))).await.unwrap();
    serde_json::from_str::<Value>(&update.json).unwrap().as_u64().unwrap()
}

async fn put(backend: &Backend, key: &str, coins: i64) {
    let arguments = json!({"id": key, "value": {"coins": coins}});
    backend.mutate(format!("{key}-{coins}"), call("put", arguments)).await.unwrap();
}

async fn results(group: &mut GroupSubscription) -> Vec<Value> {
    let update = group.next().await.unwrap();
    update.results.into_iter().map(|result| serde_json::from_str(&result.unwrap()).unwrap()).collect()
}

#[tokio::test]
async fn identical_subscriptions_share_one_evaluation_until_they_read_the_caller() {
    let directory = tempfile::tempdir().unwrap();
    // One read engine, so the evaluation counter covers every query.
    let backend = Backend::with_readers("local".into(), Box::new(open(&directory)), 1).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    put(&backend, "board", 1).await;
    let mut groups = Vec::new();
    for group in 0..3 {
        let calls = (0..4).map(|index| player(&format!("p{}", group * 4 + index))).collect();
        groups.push(backend.subscribe_group(calls).await.unwrap());
    }
    for group in &mut groups {
        assert_eq!(results(group).await, vec![json!(1); 4]);
    }
    assert_eq!(evaluated(&backend).await, 1);

    // Unrelated commits rerun nothing.
    put(&backend, "other", 5).await;
    assert_eq!(evaluated(&backend).await, 1);

    // Now the query reads its caller: every subscriber gets its own evaluation.
    put(&backend, "board", 2).await;
    for (index, group) in groups.iter_mut().enumerate() {
        let expected: Vec<_> = (0..4).map(|player| json!(format!("p{}", index * 4 + player))).collect();
        assert_eq!(results(group).await, expected);
    }
    assert_eq!(evaluated(&backend).await, 1 + 12);

    // A new subscriber with a known caller joins that caller's query.
    let mut late = backend.subscribe_group(vec![player("p0"), player("p0")]).await.unwrap();
    assert_eq!(results(&mut late).await, vec![json!("p0"); 2]);
    assert_eq!(evaluated(&backend).await, 1 + 12);
}

#[tokio::test]
async fn cached_results_that_read_the_time_rerun_with_every_commit() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::new("local".into(), Box::new(open(&directory))).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    let mut group = backend.subscribe_group(vec![call("clock", json!({})), player("p0")]).await.unwrap();
    let [before, coins] = <[Value; 2]>::try_from(results(&mut group).await).unwrap();
    assert_eq!(coins, json!(0));
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    put(&backend, "board", 1).await;
    let [after, coins] = <[Value; 2]>::try_from(results(&mut group).await).unwrap();
    assert_eq!(coins, json!(1));
    assert!(after.as_i64() > before.as_i64(), "the clock kept its old snapshot time: {before} then {after}");
}
