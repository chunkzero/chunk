//! Outgoing messages, charged against the send budget until HTTP/2 frees them.

use super::{commands::until, *};
use tonic::{codegen::http::uri::PathAndQuery, transport::Endpoint};

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_result_stays_charged_while_http2_holds_it_for_a_client_that_stopped_reading() {
    let fixture = Fixture::start().await;
    let budget = fixture.backend.send_budget().clone();
    let idle = budget.bytes();
    let endpoint = Endpoint::from_shared(fixture.endpoint.clone()).unwrap();
    let channel = endpoint.initial_stream_window_size(Some(64 * 1024)).connect().await.unwrap();
    // Takes the call's response as a stream, whose body is left unread.
    let mut client = tonic::client::Grpc::new(channel);
    client.ready().await.unwrap();
    let call = CallRequest {
        method: "big".into(),
        arguments: b"null".to_vec(),
        deployment: "test".into(),
        ..CallRequest::default()
    };
    let (path, codec) = (PathAndQuery::from_static("/chunk.sync.v1.Core/Call"), tonic_prost::ProstCodec::default());
    let response: Response<Streaming<CallResponse>> =
        client.server_streaming(authorized(call, &fixture.cli), path, codec).await.unwrap();

    until(|| budget.bytes() >= idle + BIG).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(budget.bytes() >= idle + BIG, "the result was released while HTTP/2 held it");
    drop(response);
    until(|| budget.bytes() == idle).await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn calls_are_refused_before_they_run_once_the_send_budget_is_nearly_full_but_admitted_ones_answer() {
    let mut fixture = Fixture::start().await;
    let (cli, backend) = (fixture.cli.clone(), fixture.backend.clone());
    let send = backend.send_budget().clone();
    let count = async |fixture: &mut Fixture| fixture.call(&cli, "", "get", "null").await;

    let filler = send.charge(send.available() - 512 * 1024).unwrap();
    assert_eq!(code(&fixture.call(&cli, "refused", "add", "1").await), Code::Overloaded);
    drop(filler);
    assert_eq!(result(&count(&mut fixture).await), b"0", "the refused mutation committed");

    // The spinning mutation holds the backend, so the next one is admitted, then waits while the budget runs out.
    let spin = tokio::spawn({
        let backend = backend.clone();
        let deployment = chunk_js::DeploymentId::new("test").unwrap();
        let caller = serde_json::json!({"kind": "cli"}).into();
        let call = chunk_backend::Call {
            deployment,
            function: "spin".into(),
            arguments: serde_json::Value::Null.into(),
            caller,
        };
        async move { backend.mutate("spin".into(), call).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let idle = backend.request_bytes();
    let add = tokio::spawn({
        let (mut client, cli) = (fixture.client.clone(), cli.clone());
        let message = CallRequest {
            operation_id: "admitted".into(),
            method: "add".into(),
            arguments: "1".into(),
            deployment: "test".into(),
            ..CallRequest::default()
        };
        async move { client.call(authorized(message, &cli)).await.unwrap().into_inner() }
    });
    until(|| backend.request_bytes() > idle).await;
    let overdrawn = send.overdraw(send.available() + 1);
    assert!(!spin.is_finished(), "the mutation ran before the budget ran out");
    let added = add.await.unwrap();
    assert_eq!(result(&added), b"1");
    assert!(added.position.is_some());
    drop(overdrawn);
    assert_eq!(result(&count(&mut fixture).await), b"1", "the admitted mutation's commit is missing");
    assert!(spin.await.unwrap().is_err());
    fixture.stop().await;
}

fn result(response: &CallResponse) -> &[u8] {
    match &response.outcome {
        Some(call_response::Outcome::Result(result)) => result,
        outcome => panic!("expected a result, got {outcome:?}"),
    }
}
