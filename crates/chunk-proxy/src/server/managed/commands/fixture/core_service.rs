//! A fake of core's sync service: the `gateway/proxy` topic holding one claim, the `chunk:*` claim and command
//! methods, command topics, and the deployment's root routing hook.

use super::Service;
use chunk_proto::sync::v1::{
    self as sync, ActivateResult, CallRequest, CallResponse, ClaimArguments, ClaimAssignment, ClaimPhase, ClaimRefusal,
    ClaimResult, GatewayClaim, ManifestResult, PrepareResult, SubscribeRequest, Update, WithdrawResult, call_response,
    claim_result, core_server, error::Code,
};
use prost::Message;
use std::{sync::atomic::Ordering, time::Duration};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

impl Service {
    /// Announces changed claim state to open streams, returning its position.
    pub fn publish(&self) -> sync::Position {
        self.published.send_modify(|position| *position += 1);
        self.position()
    }

    /// Ends the open gateway stream, which the gateway then follows again on a new one.
    pub fn drop_stream(&self) {
        self.superseded.notify_one();
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
            ..GatewayClaim::default()
        };
        let entry =
            sync::Entry { key: held.operation, state: Some(sync::entry::State::Value(claim.encode_to_vec().into())) };
        Update { position: Some(self.position()), snapshot: true, upserts: vec![entry], stream, ..Update::default() }
    }

    /// Runs a `chunk:*` method, or fails it with a transport error.
    async fn platform(&self, call: &CallRequest) -> Result<Result<Vec<u8>, sync::Error>, Status> {
        let operation = call.operation_id.clone();
        Ok(Ok(match call.method.as_str() {
            "chunk:claim" => {
                let arguments = ClaimArguments::decode(call.arguments.as_slice()).unwrap();
                let Some(login) = arguments.login else {
                    return Ok(self.claim_move(&operation, &arguments.deployment).await);
                };
                let mut logins = self.logins.lock().unwrap();
                logins.claims.push((operation, login.clone()));
                let placement = self.placement.lock().unwrap();
                let returned = placement.returns.clone().filter(|_| !login.decline_reconnect);
                let outcome = if let Some((deployment, destination)) = returned {
                    claim_result::Outcome::Assignment(ClaimAssignment {
                        deployment,
                        destination: Some(destination),
                        ..reservation()
                    })
                } else if logins.retired.as_ref() == Some(&login.deployment) || placement.refuses(&login.deployment) {
                    claim_result::Outcome::Refusal(ClaimRefusal::RouteAgain.into())
                } else {
                    claim_result::Outcome::Assignment(placed(&login.deployment))
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
            "chunk:commands" | "chunk:suggest" | "chunk:command" | "chunk:effect" => {
                return Ok(self.command(call).await);
            }
            _ => return Ok(Err(sync::Error { code: Code::Invalid.into(), message: "unused".into() })),
        }))
    }

    /// Runs a call that names no stream: the manifest, or its routing hook under a prepared operation ID.
    async fn unfenced(&self, call: &CallRequest) -> Result<Vec<u8>, sync::Error> {
        match call.method.as_str() {
            "chunk:manifest" => {
                let manifest = self.placement.lock().unwrap().manifests.get(&call.deployment).cloned();
                let manifest = manifest.unwrap_or_else(|| {
                    serde_json::json!({
                        "version":1, "apps":{"lobby":""}, "scopes":{"":{"parent":null}},
                        "hooks":{"shared/domains/hooks/route":{"domain":"","event":"player.route","export":"route"}}
                    })
                });
                let manifest_json = serde_json::to_vec(&manifest).unwrap();
                Ok(ManifestResult { deployment: call.deployment.clone(), manifest_json }.encode_to_vec())
            }
            "chunk:prepare" => {
                let operation_id = format!("prep:{}", self.prepared.fetch_add(1, Ordering::SeqCst));
                Ok(PrepareResult { operation_id }.encode_to_vec())
            }
            hook if hook.starts_with("shared/domains/hooks/") && hook != "shared/domains/hooks/route" => {
                let mut placement = self.placement.lock().unwrap();
                let arguments = serde_json::from_slice(&call.arguments).unwrap();
                placement.hooks.push((call.deployment.clone(), hook.into(), arguments));
                Ok(serde_json::to_vec(&serde_json::json!({"allow": !placement.denying.contains(&call.deployment)}))
                    .unwrap())
            }
            "shared/domains/hooks/route" => {
                assert!(call.operation_id.starts_with("prep:"));
                let arguments = serde_json::from_slice(&call.arguments).unwrap();
                let app = {
                    let mut placement = self.placement.lock().unwrap();
                    placement.hooks.push((call.deployment.clone(), call.method.clone(), arguments));
                    let manifest = placement.manifests.get(&call.deployment);
                    let apps = manifest.and_then(|manifest| manifest["apps"].as_object());
                    apps.and_then(|apps| apps.keys().next().cloned()).unwrap_or_else(|| "lobby".into())
                };
                let unroutable = self.logins.lock().unwrap().unroutable.take();
                if let Some(unroutable) = unroutable {
                    let _ = unroutable.await;
                    return Err(sync::Error { code: Code::Unavailable.into(), message: "routing failed".into() });
                }
                let route =
                    serde_json::json!({"key":app,"session_type":format!("{app}/default"),"machine_profile":"local"});
                Ok(serde_json::to_vec(&route).unwrap())
            }
            _ => Err(sync::Error { code: Code::Invalid.into(), message: "unused".into() }),
        }
    }

    async fn claim_move(&self, operation: &str, approved: &str) -> Result<Vec<u8>, sync::Error> {
        let (error, stall) = {
            let mut movement = self.movement.lock().unwrap();
            assert_eq!(movement.pending.as_ref().map(|pending| pending.operation_id.as_str()), Some(operation));
            movement.attempts += 1;
            (movement.error.clone(), movement.stall_retries && movement.attempts > 1)
        };
        if stall {
            tokio::time::sleep(crate::server::managed::WAIT_TIMEOUT).await;
        }
        if let Some(error) = error {
            return Err(error);
        }
        let outcome = if self.placement.lock().unwrap().refuses(approved) {
            claim_result::Outcome::Refusal(ClaimRefusal::RouteAgain.into())
        } else {
            claim_result::Outcome::Assignment(placed(approved))
        };
        Ok(ClaimResult { outcome: Some(outcome) }.encode_to_vec())
    }
}

impl super::Placement {
    /// Whether a claim admitted in `deployment` is refused, because another is current.
    fn refuses(&self, deployment: &str) -> bool {
        !deployment.is_empty() && self.current.as_ref().is_some_and(|current| current != deployment)
    }
}

/// A reservation in `deployment`, or in the fixture's own when it is empty.
fn placed(deployment: &str) -> ClaimAssignment {
    let deployment = if deployment.is_empty() { "deployment" } else { deployment };
    ClaimAssignment { deployment: deployment.into(), ..reservation() }
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
        if call.stream.is_empty() {
            let outcome = match self.unfenced(&call).await {
                Ok(result) => call_response::Outcome::Result(result),
                Err(error) => call_response::Outcome::Error(error),
            };
            return Ok(Response::new(CallResponse { position: None, outcome: Some(outcome) }));
        }
        if call.stream == *self.stream.lock().unwrap() && self.supersede.load(Ordering::SeqCst) {
            self.superseded.notify_one();
            while call.stream == *self.stream.lock().unwrap() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            self.supersede.store(false, Ordering::SeqCst);
            self.superseded.notify_one();
        }
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
        if let Some(operation) = request.get_ref().topic.strip_prefix("command/") {
            if self.reopening.load(Ordering::SeqCst) {
                self.reopened.notify_one();
                self.reopen.notified().await;
            }
            return Ok(Response::new(self.follow(operation, &request.get_ref().arguments)));
        }
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
        let held = self.supersede.load(Ordering::SeqCst);
        tokio::spawn(async move {
            if held {
                service.superseded.notified().await;
                // Lets the stopped call's response reach the gateway first.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            loop {
                published.borrow_and_update();
                if service.watch_down.load(Ordering::SeqCst)
                    || sender.send(Ok(service.snapshot(std::mem::take(&mut stream)))).await.is_err()
                {
                    return;
                }
                tokio::select! {
                    () = service.watches.cancelled() => return,
                    () = service.superseded.notified() => return,
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
        deployment: "deployment".into(),
        destination: None,
    }
}
