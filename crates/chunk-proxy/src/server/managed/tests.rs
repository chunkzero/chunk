//! Withdrawing the claims other processes under this gateway's ID left open, against a fake core.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
    time::Duration,
};

use chunk_proto::sync::v1::{
    self as sync, CallRequest, CallResponse, ClaimPhase, GatewayArguments, GatewayClaim, Position, SubscribeRequest,
    Update, WithdrawResult, call_response,
    core_server::{Core, CoreServer},
    error::Code,
};
use prost::Message;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status};

use super::withdraw_inherited;
use crate::{GatewayCredential, PlatformTarget, server::platform::Platform};

/// A core holding one gateway's claims, which fails the first withdrawal of each operation in `failing`.
#[derive(Clone)]
struct FakeCore {
    claims: Arc<Mutex<BTreeMap<String, GatewayClaim>>>,
    revision: Arc<watch::Sender<u64>>,
    failing: Arc<Mutex<BTreeSet<String>>>,
    withdrawals: Arc<Mutex<Vec<String>>>,
}

impl FakeCore {
    fn new() -> Self {
        Self {
            claims: Arc::default(),
            revision: Arc::new(watch::Sender::new(0)),
            failing: Arc::default(),
            withdrawals: Arc::default(),
        }
    }

    /// Commits `operation`'s claim as `change` leaves it.
    fn commit(&self, operation: &str, change: impl FnOnce(&mut GatewayClaim)) {
        self.revision.send_modify(|revision| {
            *revision += 1;
            let mut claims = self.claims.lock().unwrap();
            let claim = claims
                .entry(operation.into())
                .or_insert_with(|| GatewayClaim { generation: Some(position(*revision)), ..GatewayClaim::default() });
            change(claim);
        });
    }

    fn open(&self, operation: &str, connection: String, phase: ClaimPhase) {
        self.commit(operation, |claim| {
            claim.connection_id = connection;
            claim.set_phase(phase);
        });
    }

    fn withdrawals(&self) -> Vec<String> {
        self.withdrawals.lock().unwrap().clone()
    }

    /// Every update is a snapshot, as core may send one at any time.
    fn snapshot(&self) -> Update {
        let position = position(*self.revision.borrow());
        let claims = self.claims.lock().unwrap();
        let upserts = claims.iter().map(|(operation, claim)| sync::Entry {
            key: operation.clone(),
            state: Some(sync::entry::State::Value(claim.encode_to_vec())),
        });
        let upserts = upserts.collect();
        Update { position: Some(position), snapshot: true, upserts, stream: "stream".into(), ..Update::default() }
    }
}

fn position(revision: u64) -> Position {
    Position { epoch: 1, revision }
}

#[tonic::async_trait]
impl Core for FakeCore {
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        let call = request.into_inner();
        assert_eq!((call.method.as_str(), call.stream.as_str()), ("chunk:withdraw", "stream"));
        self.withdrawals.lock().unwrap().push(call.operation_id.clone());
        let outcome = if self.failing.lock().unwrap().remove(&call.operation_id) {
            let error = sync::Error { code: Code::Unavailable.into(), message: "unavailable".into() };
            call_response::Outcome::Error(error)
        } else {
            self.commit(&call.operation_id, |claim| claim.set_phase(ClaimPhase::Withdrawing));
            call_response::Outcome::Result(WithdrawResult { unknown: false }.encode_to_vec())
        };
        let position = Some(position(*self.revision.borrow()));
        Ok(Response::new(CallResponse { position, outcome: Some(outcome) }))
    }

    type SubscribeStream = ReceiverStream<Result<Update, Status>>;
    async fn subscribe(&self, request: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, Status> {
        let arguments = GatewayArguments::decode(request.get_ref().arguments.as_slice()).unwrap();
        assert!(!arguments.instance.is_empty());
        let (sender, receiver) = mpsc::channel(1);
        let (claims, mut revision) = (self.clone(), self.revision.subscribe());
        tokio::spawn(async move {
            loop {
                revision.borrow_and_update();
                if sender.send(Ok(claims.snapshot())).await.is_err() || revision.changed().await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}

#[tokio::test]
async fn inherited_claims_are_withdrawn_through_a_failure_and_as_they_appear_but_own_ones_never_are() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let core = FakeCore::new();
    let platform = Platform::new(PlatformTarget {
        core: format!("http://{}", listener.local_addr().unwrap()),
        gateway: GatewayCredential { id: "proxy".into(), credential: "gateway".into() },
        deployment: "test".into(),
    })
    .unwrap();
    let server = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(CoreServer::new(core.clone()))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );

    // An earlier process's claim, whose first withdrawal fails, and a claim of a move of this process's player.
    core.open("earlier", "earlier/connection".into(), ClaimPhase::Arrived);
    core.open("moved", platform.connection_id(), ClaimPhase::Reserved);
    core.failing.lock().unwrap().insert("earlier".into());
    let (ready, withdrawn) = oneshot::channel();
    let cleanup = tokio::spawn(async move { withdraw_inherited(&platform, ready).await });
    tokio::time::timeout(Duration::from_secs(10), withdrawn).await.unwrap().unwrap();
    assert_eq!(core.withdrawals(), ["earlier", "earlier"]);

    // A claim of the earlier process that commits later, as one its call made before this process took over may.
    core.open("late", "earlier/late".into(), ClaimPhase::Reserved);
    let late = async {
        while core.withdrawals().len() < 3 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), late).await.unwrap();
    assert_eq!(core.withdrawals(), ["earlier", "earlier", "late"]);
    cleanup.abort();
    server.abort();
}
