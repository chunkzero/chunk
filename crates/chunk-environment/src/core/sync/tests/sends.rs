//! Outgoing messages, charged against the send budget until HTTP/2 frees them.

use super::{commands::until, *};
use tonic::transport::Endpoint;

/// What a message of the `big` query's result holds at least.
const BIG: usize = 900 * 1024;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn messages_stay_charged_while_http2_holds_them_for_a_client_that_stopped_reading() {
    let mut fixture = Fixture::start().await;
    let cli = fixture.cli.clone();
    let budget = fixture.backend.send_budget().clone();
    let idle = budget.bytes();
    let endpoint = Endpoint::from_shared(fixture.endpoint.clone()).unwrap();
    let channel = endpoint.initial_stream_window_size(Some(64 * 1024)).connect().await.unwrap();
    let subscription = SubscribeRequest {
        topic: "queries".into(),
        arguments: br#"{"big": {"function": "big"}}"#.to_vec(),
        deployment: "test".into(),
        ..SubscribeRequest::default()
    };
    let updates = CoreClient::new(channel).subscribe(authorized(subscription, &cli)).await.unwrap().into_inner();
    for count in 0..16 {
        fixture.call(&cli, &format!("add-{count}"), "add", "1").await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // One message is in h2's send buffer, one in hyper's, and one waits for the stream to yield it.
    until(|| budget.bytes() >= idle + 3 * BIG).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let held = budget.bytes() - idle;
    assert!((3 * BIG..4 * BIG).contains(&held), "{held} bytes charged");
    drop(updates);
    until(|| budget.bytes() == idle).await;
    fixture.stop().await;
}
