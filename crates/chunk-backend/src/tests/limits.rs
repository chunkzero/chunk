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

#[tokio::test]
async fn oversized_mutation_operation_ids_are_rejected_before_enqueue() {
    let (backend, mut incoming, memory) = Backend::held_ingress();
    let mut mutation = Box::pin(backend.mutate("x".repeat(1024 * 1024), call("bump", json!({"id": "p"}))));
    std::future::poll_fn(|cx| {
        assert!(
            matches!(mutation.as_mut().poll(cx), std::task::Poll::Ready(Err(Error::Invalid("operation identity")))),
            "oversized mutation operation ID was retained before validation"
        );
        std::task::Poll::Ready(())
    })
    .await;
    assert!(incoming.try_recv().is_err(), "invalid operation must never enter the actor queue");
    assert_eq!(memory.available_permits(), crate::limits::REQUEST_BYTES);
}

pub(crate) fn assert_request_charge(memory: &tokio::sync::Semaphore, payload: usize) {
    let charged = crate::limits::REQUEST_BYTES - memory.available_permits();
    let required = crate::limits::REQUEST_OVERHEAD + payload;
    assert!(charged >= required, "retained request charged {charged} bytes, but needs at least {required} bytes");
}

#[tokio::test]
async fn mutation_admission_charges_retained_operation_ids() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let operation = "x".repeat(256);
    let call = call("bump", json!({"id": "p"}));
    let bytes = operation.len() + call.arguments.as_str().len() + call.caller.as_str().len();
    let mut mutation = Box::pin(backend.mutate(operation, call));
    super::pending(mutation.as_mut()).await;
    assert_request_charge(&memory, bytes);
}

#[tokio::test]
async fn action_status_admission_charges_retained_ids_and_callers() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let id = crate::ActionId { incarnation: "test-incarnation".into(), sequence: 1 };
    let caller: chunk_js::Json = json!({"player": "x".repeat(4096)}).into();
    let bytes = id.incarnation.len() + caller.as_str().len();
    let mut status = Box::pin(backend.action_status(id, caller));
    super::pending(status.as_mut()).await;
    assert_request_charge(&memory, bytes);
}

#[tokio::test]
async fn job_status_admission_charges_retained_ids_and_callers() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let id = "j".repeat(256);
    let caller: chunk_js::Json = json!({"player": "x".repeat(4096)}).into();
    let bytes = id.len() + caller.as_str().len();
    let mut status = Box::pin(backend.job(id, caller));
    super::pending(status.as_mut()).await;
    assert_request_charge(&memory, bytes);
}

#[tokio::test]
async fn forget_job_admission_charges_retained_ids_and_callers() {
    let (backend, _incoming, memory) = Backend::held_ingress();
    let id = "j".repeat(256);
    let caller: chunk_js::Json = json!({"player": "x".repeat(4096)}).into();
    let bytes = id.len() + caller.as_str().len();
    let mut forgotten = Box::pin(backend.forget_job(id, caller));
    super::pending(forgotten.as_mut()).await;
    assert_request_charge(&memory, bytes);
}

#[test]
fn cgroup_limits_take_the_strictest_group_up_to_the_controller_mount() {
    const GIB: usize = 1024 * 1024 * 1024;
    let limit = |mountinfo: &str, groups: &str, files: &[(&str, &str)]| {
        crate::limits::cgroup_limit(mountinfo, groups, |path| {
            files.iter().find(|(file, _)| std::path::Path::new(file) == path).map(|(_, text)| (*text).to_owned())
        })
    };
    let v2 = "30 23 0:26 / /sys/fs/cgroup rw,nosuid shared:4 - cgroup2 cgroup2 rw,nsdelegate\n";
    let parent = ("/sys/fs/cgroup/parent/memory.max", "2147483648\n");
    assert_eq!(
        limit(v2, "0::/parent/leaf\n", &[parent, ("/sys/fs/cgroup/parent/leaf/memory.max", "4294967296\n")]),
        Some(2 * GIB)
    );
    assert_eq!(
        limit(v2, "0::/parent/leaf\n", &[parent, ("/sys/fs/cgroup/parent/leaf/memory.max", "max\n")]),
        Some(2 * GIB)
    );
    // A v1 memory controller mounted from inside the hierarchy, beside other controllers.
    let v1 = "34 25 0:29 / /sys/fs/cgroup/cpu rw shared:14 - cgroup cgroup rw,cpu,cpuacct\n\
              35 25 0:30 /pods /mnt/memory rw,nosuid shared:15 - cgroup cgroup rw,memory\n";
    let groups = "5:cpu,cpuacct:/elsewhere\n4:memory:/pods/pod\n1:name=systemd:/pods/pod\n";
    assert_eq!(
        limit(
            v1,
            groups,
            &[
                ("/sys/fs/cgroup/cpu/memory.limit_in_bytes", "1"),
                ("/mnt/memory/memory.limit_in_bytes", "9223372036854771712"),
                ("/mnt/memory/pod/memory.limit_in_bytes", "1073741824"),
            ]
        ),
        Some(GIB)
    );
    assert_eq!(limit(v2, "0::/parent/leaf\n", &[]), None);
}
