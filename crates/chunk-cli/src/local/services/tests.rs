use std::pin::Pin;

use chunk_proto::sync::v1::{CallRequest, CallResponse, Entry, SubscribeRequest, Update, core_server, entry::State};
use prost::Message;
use tokio::sync::mpsc;
use tokio_stream::{
    Stream,
    wrappers::{ReceiverStream, TcpListenerStream},
};
use tonic::{Request, Response, Status};

use super::*;

type Updates = mpsc::Sender<Result<Update, Status>>;

/// A core that hands each subscription's topic and update sender to the test.
struct Scripted(mpsc::UnboundedSender<(String, Updates)>);

#[tonic::async_trait]
impl core_server::Core for Scripted {
    async fn call(&self, _: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        Err(Status::unimplemented("call"))
    }

    type SubscribeStream = Pin<Box<dyn Stream<Item = Result<Update, Status>> + Send>>;

    async fn subscribe(&self, request: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, Status> {
        let (sender, receiver) = mpsc::channel(4);
        self.0.send((request.into_inner().topic, sender)).map_err(|_| Status::unavailable("the test ended"))?;
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// Answers the next `nodes` and `players` subscriptions with snapshots, `nodes` holding `host` alone. Returns the
/// `nodes` and `players` senders, which keep their streams open.
async fn serve(subscribed: &mut mpsc::UnboundedReceiver<(String, Updates)>, host: &str) -> (Updates, Updates) {
    let mut senders = BTreeMap::new();
    while senders.len() < 2 {
        let (topic, sender) = subscribed.recv().await.unwrap();
        senders.insert(topic, sender);
    }
    let (nodes, players) = (senders.remove("nodes").unwrap(), senders.remove("players").unwrap());
    let node = Entry { key: host.into(), state: Some(State::Value(Node::default().encode_to_vec().into())) };
    nodes.send(Ok(Update { snapshot: true, upserts: vec![node], ..Update::default() })).await.unwrap();
    players.send(Ok(Update { snapshot: true, ..Update::default() })).await.unwrap();
    (nodes, players)
}

fn hosts(observed: Option<&Observed>) -> Option<Vec<&str>> {
    observed.map(|observed| observed.nodes.keys().map(String::as_str).collect())
}

#[tokio::test]
async fn a_broken_subscription_clears_the_view_then_resubscribes_from_a_new_snapshot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (subscriptions, mut subscribed) = mpsc::unbounded_channel();
    let server = tonic::transport::Server::builder().add_service(core_server::CoreServer::new(Scripted(subscriptions)));
    tokio::spawn(server.serve_with_incoming(TcpListenerStream::new(listener)));
    let (mut observed, _guard) = observe(ControlConnection { endpoint, token: "token".into() });

    let recovered = async {
        let (nodes, _players) = serve(&mut subscribed, "first").await;
        observed.wait_for(|observed| hosts(observed.as_ref()) == Some(vec!["first"])).await.unwrap();
        nodes.send(Err(Status::unavailable("core restarted"))).await.unwrap();
        observed.wait_for(Option::is_none).await.unwrap();
        let _open = serve(&mut subscribed, "second").await;
        observed.wait_for(|observed| hosts(observed.as_ref()) == Some(vec!["second"])).await.unwrap();
    };
    tokio::time::timeout(Duration::from_secs(10), recovered).await.expect("the view recovered in time");
}
