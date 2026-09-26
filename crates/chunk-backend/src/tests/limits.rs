use std::time::Duration;

use chunk_js::Limits;
use serde_json::json;

use super::{call, id, open};
use crate::{Backend, Error, Limit};

const SOURCE: &str = r"
export function slow(ctx, args) { let total = 0; for (let i = 0; i < args.n; i++) total += i; return total; }
export function fast() { return 1; }
";

#[tokio::test]
async fn queries_are_refused_once_queued_ones_wait_too_long_for_a_read_engine() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Backend::with_readers("local".into(), Box::new(open(&directory)), 1).unwrap();
    backend.register(id(), SOURCE.into(), Limits::default()).await.unwrap();
    let slow: Vec<_> = (0..40)
        .map(|_| {
            let backend = backend.clone();
            tokio::spawn(async move { backend.query(call("slow", json!({"n": 50_000_000}))).await })
        })
        .collect();
    // Probes that are admitted wait behind the slow queries; dropping them cancels.
    let mut refused = None;
    for _ in 0..200 {
        if let Ok(Err(error)) =
            tokio::time::timeout(Duration::from_millis(20), backend.query(call("fast", json!({})))).await
        {
            refused = Some(error);
            break;
        }
    }
    assert!(matches!(refused, Some(Error::Overloaded(Limit::ReadQueue))), "{refused:?}");
    for query in slow {
        let _ = query.await.unwrap();
    }
    backend.query(call("fast", json!({}))).await.unwrap();
}
