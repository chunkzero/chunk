//! Session methods a JVM registered over sync runs through its topic, within its method budget.

use super::{jvm::Launches, jvm_effects::arrive, *};
use chunk_proto::v1::SessionMethodPhase;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_methods_run_and_cancel_through_the_topic() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, assignment) = arrive(&fixture, &launches).await;
    let control = fixture.control.clone();
    let captured = control.capture_session(assignment.claim.as_ref().unwrap()).unwrap();
    let timeout = Duration::from_secs(10);

    let score = control.prepare_session_method(&captured, "score", serde_json::json!({}), timeout).unwrap();
    let result = control.call_session_method(&score, &CancellationToken::new()).await.unwrap();
    assert_eq!((result.phase(), result.result_json.as_str()), (SessionMethodPhase::Completed, "7"));
    // A retry returns the recorded result rather than running the method again.
    assert_eq!(control.call_session_method(&score, &CancellationToken::new()).await.unwrap(), result);

    let hold = control.prepare_session_method(&captured, "hold", serde_json::json!({}), timeout).unwrap();
    let cancellation = CancellationToken::new();
    let call = {
        let (control, hold, cancellation) = (control.clone(), hold.clone(), cancellation.clone());
        tokio::spawn(async move { control.call_session_method(&hold, &cancellation).await })
    };
    while !jvm.sees(&format!("method/{}", hold.operation_id())) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancellation.cancel();
    assert_eq!(call.await.unwrap().unwrap().phase(), SessionMethodPhase::Cancelled);
    assert!(jvm.runnable(hold.operation_id()));

    // A call cancelled before it reaches the topic is never runnable there.
    let early = control.prepare_session_method(&captured, "hold", serde_json::json!({}), timeout).unwrap();
    let result = control.call_session_method(&early, &cancellation).await.unwrap();
    assert_eq!(result.phase(), SessionMethodPhase::Cancelled);
    assert!(!jvm.runnable(early.operation_id()));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvms_method_budget_rejects_new_calls_and_keeps_charging_unanswered_ones() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, assignment) = arrive(&fixture, &launches).await;
    let control = fixture.control.clone();
    let captured = control.capture_session(assignment.claim.as_ref().unwrap()).unwrap();
    let prepare =
        |name, arguments| control.prepare_session_method(&captured, name, arguments, Duration::from_secs(10)).unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    // A method the JVM never answers stays on its topic after its outcome turned unknown.
    let stuck = prepare("stuck", serde_json::json!({}));
    assert_eq!(control.call_session_method(&stuck, &cancelled).await.unwrap().phase(), SessionMethodPhase::Unknown);

    // Pending methods with large arguments reach the byte limit well before the count limit.
    let text = "x".repeat(16 * 1024);
    let later = CancellationToken::new();
    let mut pending = Vec::new();
    let rejected = loop {
        let method = prepare("hold", serde_json::json!({ "text": text }));
        let call = {
            let (control, method, later) = (control.clone(), method.clone(), later.clone());
            tokio::spawn(async move { control.call_session_method(&method, &later).await })
        };
        let key = format!("method/{}", method.operation_id());
        while !jvm.sees(&key) && !call.is_finished() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        if call.is_finished() {
            break call.await.unwrap().unwrap_err();
        }
        pending.push(call);
    };
    assert!(matches!(rejected, chunk_control::Error::Capacity), "{rejected:?}");
    assert!(pending.len() < 200, "{} pending methods fit", pending.len());
    // Once answered, they hold only their small results.
    later.cancel();
    let mut answered = pending.len();
    for call in pending {
        assert_eq!(call.await.unwrap().unwrap().phase(), SessionMethodPhase::Cancelled);
    }

    // Answered methods fill the rest of the 256 the budget holds, which then rejects new ones.
    let rejected = loop {
        match control.call_session_method(&prepare("hold", serde_json::json!({})), &cancelled).await {
            Ok(result) => assert_eq!(result.phase(), SessionMethodPhase::Cancelled),
            Err(error) => break error,
        }
        answered += 1;
        assert!(answered < 256, "the budget admitted too many methods");
    };
    assert!(matches!(rejected, chunk_control::Error::Capacity), "{rejected:?}");
    assert_eq!(answered, 255);
    assert!(jvm.sees(&format!("method/{}", stuck.operation_id())));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_whose_stop_failed_keeps_its_method_history() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, assignment) = arrive(&fixture, &launches).await;
    let control = fixture.control.clone();
    let captured = control.capture_session(assignment.claim.as_ref().unwrap()).unwrap();
    let timeout = Duration::from_secs(10);
    let score = control.prepare_session_method(&captured, "score", serde_json::json!({}), timeout).unwrap();
    let result = control.call_session_method(&score, &CancellationToken::new()).await.unwrap();
    assert_eq!(result.phase(), SessionMethodPhase::Completed);
    let stuck = control.prepare_session_method(&captured, "stuck", serde_json::json!({}), timeout).unwrap();
    let call = {
        let (control, stuck) = (control.clone(), stuck.clone());
        tokio::spawn(async move { control.call_session_method(&stuck, &CancellationToken::new()).await })
    };
    let stuck = format!("method/{}", stuck.operation_id());
    while !jvm.sees(&stuck) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // The host fails to stop the JVM, which may still be running.
    launches.0.lock().unwrap().refusals = 1;
    assert!(control.shutdown().await.is_err());
    // Reconnecting, the JVM is still asked about the pending method, and a retry of the completed one gets its result
    // without running it again.
    jvm.forget_runnable();
    jvm.follow().await;
    assert!(jvm.sees(&stuck));
    assert_eq!(control.call_session_method(&score, &CancellationToken::new()).await.unwrap(), result);
    assert!(!jvm.runnable(score.operation_id()));
    call.abort();
    fixture.stop().await;
}
