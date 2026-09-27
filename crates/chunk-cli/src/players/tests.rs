use std::pin::Pin;

use chunk_proto::sync::v1::{
    CallRequest, CallResponse, Entry, SubscribeRequest, Update, call_response::Outcome, core_server, entry::State,
};
use prost::Message;
use tokio_stream::{Stream, StreamExt, wrappers::TcpListenerStream};
use tonic::{Request, Response, Status};

use super::*;

/// A core whose drain of `host` passed its deadline a minute ago, and whose `nodes` show it in the given phase.
struct Expired(NodePhase);

#[tonic::async_trait]
impl core_server::Core for Expired {
    async fn call(&self, _: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        let drained = DrainResult { host: "host".into(), deadline_ms: now_ms() - 60_000 };
        let outcome = Some(Outcome::Result(drained.encode_to_vec()));
        Ok(Response::new(CallResponse { outcome, ..CallResponse::default() }))
    }

    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<Update, Status>> + Send>>;

    /// Answers after a moment, as a busy core does, but well within the call timeout.
    async fn subscribe(&self, _: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, Status> {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let node = Node { phase: self.0.into(), ..Node::default() };
        let entry = Entry { key: "host".into(), state: Some(State::Value(node.encode_to_vec())) };
        let snapshot = Update { snapshot: true, upserts: vec![entry], ..Update::default() };
        Ok(Response::new(Box::pin(tokio_stream::once(Ok(snapshot)).chain(tokio_stream::pending()))))
    }
}

async fn retry_drain(phase: NodePhase) -> io::Result<()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let server = tonic::transport::Server::builder().add_service(core_server::CoreServer::new(Expired(phase)));
    tokio::spawn(server.serve_with_incoming(TcpListenerStream::new(listener)));
    let state = tempfile::tempdir()?;
    let control_file = state.path().join("control.json");
    let connection = chunk_contract::ControlConnection { endpoint, token: "token".into() };
    std::fs::write(&control_file, serde_json::to_vec(&connection)?)?;
    let action = Action::Drain { timeout_seconds: 10 };
    run(Options { control_file, player: uuid::Uuid::new_v4(), operation: Some(uuid::Uuid::new_v4()), action }).await
}

#[tokio::test]
async fn a_drain_retried_after_its_deadline_reports_whether_the_host_stopped() {
    retry_drain(NodePhase::Stopped).await.unwrap();
    let error = retry_drain(NodePhase::Draining).await.unwrap_err();
    assert!(error.to_string().contains("unresolved"), "{error}");
}
