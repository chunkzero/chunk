//! A fake of core's sync service: the `gateway/proxy` topic holding one claim, and the `chunk:*` claim methods.

use super::Service;
use chunk_proto::sync::v1::{
    self as sync, ActivateResult, CallRequest, CallResponse, ClaimArguments, ClaimAssignment, ClaimPhase, ClaimRefusal,
    ClaimResult, GatewayClaim, SubscribeRequest, Update, WithdrawResult, call_response, claim_result, core_server,
    error::Code,
};
use prost::Message;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

impl Service {
    /// Announces changed claim state to open streams, returning its position.
    pub fn publish(&self) -> sync::Position {
        self.published.send_modify(|position| *position += 1);
        self.position()
    }

    fn position(&self) -> sync::Position {
        sync::Position { epoch: 1, revision: *self.published.borrow() }
    }

    fn snapshot(&self, stream: String) -> Update {
        let held = self.claim.lock().unwrap().clone();
        let claim = GatewayClaim {
            generation: Some(held.generation),
            phase: held.phase.into(),
            pending_move: self.movement.lock().unwrap().pending.clone(),
        };
        let entry = sync::Entry { key: held.operation, state: Some(sync::entry::State::Value(claim.encode_to_vec())) };
        Update { position: Some(self.position()), snapshot: true, upserts: vec![entry], stream, ..Update::default() }
    }

    /// Runs a `chunk:*` method, or fails it with a transport error.
    async fn platform(&self, call: &CallRequest) -> Result<Result<Vec<u8>, sync::Error>, Status> {
        let operation = call.operation_id.clone();
        Ok(Ok(match call.method.as_str() {
            "chunk:claim" => {
                let Some(login) = ClaimArguments::decode(call.arguments.as_slice()).unwrap().login else {
                    return Ok(self.claim_move(&operation).await);
                };
                let mut logins = self.logins.lock().unwrap();
                logins.claims.push((operation, login.clone()));
                let outcome = if logins.retired.as_ref() == Some(&login.deployment) {
                    claim_result::Outcome::Refusal(ClaimRefusal::RouteAgain.into())
                } else {
                    claim_result::Outcome::Assignment(reservation())
                };
                ClaimResult { outcome: Some(outcome) }.encode_to_vec()
            }
            "chunk:activate" => {
                self.activations.fetch_add(1, Ordering::SeqCst);
                let waits =
                    self.roster_waits.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |waits| waits.checked_sub(1));
                ActivateResult { waiting: waits.is_ok() }.encode_to_vec()
            }
            "chunk:withdraw" => {
                self.logins.lock().unwrap().cancels.push(operation);
                WithdrawResult::default().encode_to_vec()
            }
            "chunk:abandon_move" => {
                let mut movement = self.movement.lock().unwrap();
                movement.reports += 1;
                if std::mem::take(&mut movement.lose_report) {
                    return Err(Status::unavailable("lost failure report"));
                }
                let expected = movement.pending.as_ref().map(|pending| &pending.operation_id);
                let expected = expected.or_else(|| movement.failure.as_ref().map(|(operation, _)| operation));
                assert_eq!(expected, Some(&operation));
                let reason = sync::AbandonMoveArguments::decode(call.arguments.as_slice()).unwrap().reason;
                movement.pending = None;
                movement.failure = Some((operation, reason));
                drop(movement);
                self.publish();
                WithdrawResult::default().encode_to_vec()
            }
            _ => return Ok(Err(sync::Error { code: Code::Invalid.into(), message: "unused".into() })),
        }))
    }

    async fn claim_move(&self, operation: &str) -> Result<Vec<u8>, sync::Error> {
        let (error, stall) = {
            let mut movement = self.movement.lock().unwrap();
            assert_eq!(movement.pending.as_ref().map(|pending| pending.operation_id.as_str()), Some(operation));
            movement.attempts += 1;
            (movement.error.clone(), movement.stall_retries && movement.attempts > 1)
        };
        if stall {
            tokio::time::sleep(crate::server::managed::WAIT_TIMEOUT).await;
        }
        Err(error.unwrap_or_else(|| sync::Error { code: Code::Invalid.into(), message: "unused".into() }))
    }
}

/// The claim the gateway's topic holds.
#[derive(Clone)]
pub(in crate::server::managed::commands) struct Held {
    pub operation: String,
    pub generation: sync::Position,
    pub phase: ClaimPhase,
}

#[tonic::async_trait]
impl core_server::Core for Service {
    async fn call(&self, request: Request<CallRequest>) -> Result<Response<CallResponse>, Status> {
        super::auth(&request, "gateway")?;
        let call = request.into_inner();
        let outcome = if call.stream == *self.stream.lock().unwrap() {
            self.platform(&call).await?
        } else {
            Err(sync::Error { code: Code::Stopped.into(), message: "superseded".into() })
        };
        let (position, outcome) = match outcome {
            Ok(result) => (Some(self.position()), call_response::Outcome::Result(result)),
            Err(error) => (None, call_response::Outcome::Error(error)),
        };
        Ok(Response::new(CallResponse { position, outcome: Some(outcome) }))
    }

    type SubscribeStream = ReceiverStream<Result<Update, Status>>;
    async fn subscribe(&self, request: Request<SubscribeRequest>) -> Result<Response<Self::SubscribeStream>, Status> {
        super::auth(&request, "gateway")?;
        assert_eq!(request.get_ref().topic, "gateway/proxy");
        if self.watch_down.load(Ordering::SeqCst) {
            self.refused_watches.fetch_add(1, Ordering::SeqCst);
            return Err(Status::unavailable("topic unavailable"));
        }
        let mut stream = format!("stream-{}", self.streams.fetch_add(1, Ordering::SeqCst));
        self.stream.lock().unwrap().clone_from(&stream);
        let (sender, receiver) = mpsc::channel(1);
        let service = self.clone();
        let mut published = self.published.subscribe();
        tokio::spawn(async move {
            loop {
                published.borrow_and_update();
                if service.watch_down.load(Ordering::SeqCst)
                    || sender.send(Ok(service.snapshot(std::mem::take(&mut stream)))).await.is_err()
                {
                    return;
                }
                tokio::select! {
                    () = service.watches.cancelled() => return,
                    () = sender.closed() => return,
                    _ = published.changed() => {}
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(receiver)))
    }
}
/// A reservation on a runtime whose protocol no client speaks.
fn reservation() -> ClaimAssignment {
    ClaimAssignment {
        generation: Some(sync::Position { epoch: 1, revision: 1 }),
        session: "session".into(),
        protocol: 0,
        endpoint: "127.0.0.1:1".into(),
        capability: vec![0; 32],
    }
}
