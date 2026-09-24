//! Synthetic runtime RPCs live in the generator process, outside target CPU/RSS.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use chunk_proto::v1::{
    ConfigurationRequest, ConfigurationResponse, DeliveryInventory, DeliveryPhase, DeploymentRef, PlayerDelivery,
    PlayerPreparation, PlayerWithdrawal, ProcessHealth, ProcessIdentity, ProcessInventory, SessionCommand,
    SessionInventory, SessionPhase,
    gameplay_server::{Gameplay, GameplayServer},
    node_control_server::{NodeControl, NodeControlServer},
    process_control_server::{ProcessControl, ProcessControlServer},
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

pub fn identity(id: &str) -> ProcessIdentity {
    ProcessIdentity {
        deployment: Some(DeploymentRef { environment: "bench".into(), deployment: "bench".into() }),
        runtime_id: id.into(),
        process_id: format!("jvm-{id}"),
        generation: 1,
        machine_profile: "bench".into(),
        artifact_digest: "bench".into(),
        app_id: "bench".into(),
    }
}

#[derive(Default)]
struct Runtime {
    sessions: BTreeMap<String, SessionInventory>,
    deliveries: BTreeMap<String, DeliveryInventory>,
    ticks: u64,
}

#[derive(Clone, Default)]
pub struct Runtimes(Arc<Mutex<BTreeMap<String, Runtime>>>);

impl Runtimes {
    pub async fn serve(self, listener: TcpListener, stop: CancellationToken) -> Result<()> {
        tonic::transport::Server::builder()
            .add_service(GameplayServer::new(self.clone()))
            .add_service(ProcessControlServer::new(self.clone()))
            .add_service(NodeControlServer::new(self))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop.cancelled_owned())
            .await?;
        Ok(())
    }

    fn call<T, R>(
        &self,
        request: Request<T>,
        f: impl FnOnce(ProcessIdentity, &mut Runtime, T) -> Result<R, Status>,
    ) -> Result<Response<R>, Status> {
        let id = request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer bench-"))
            .filter(|id| !id.is_empty())
            .ok_or_else(|| Status::unauthenticated("benchmark runtime credential"))?;
        let process = identity(id);
        let mut runtimes = self.0.lock().map_err(|_| Status::internal("fixture lock poisoned"))?;
        let runtime = runtimes.entry(id.into()).or_default();
        f(process, runtime, request.into_inner()).map(Response::new)
    }
}

#[tonic::async_trait]
impl ProcessControl for Runtimes {
    async fn inventory(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessInventory>, Status> {
        self.call(request, |identity, runtime, requested| {
            if identity != requested {
                return Err(Status::failed_precondition("runtime identity"));
            }
            Ok(ProcessInventory {
                identity: Some(identity),
                tick_count: runtime.ticks,
                sessions: runtime.sessions.values().cloned().collect(),
                deliveries: runtime.deliveries.values().cloned().collect(),
                draining: false,
            })
        })
    }

    async fn create_session(&self, request: Request<SessionCommand>) -> Result<Response<SessionInventory>, Status> {
        self.call(request, |identity, runtime, command| {
            if command.identity.as_ref() != Some(&identity) {
                return Err(Status::failed_precondition("session runtime identity"));
            }
            let session = command.session.ok_or_else(|| Status::invalid_argument("session"))?;
            let inventory = SessionInventory {
                session: Some(session.clone()),
                generation: command.generation,
                session_type: command.session_type,
                capacity: command.capacity,
                phase: SessionPhase::Ready as i32,
                ..Default::default()
            };
            runtime.sessions.insert(session.id, inventory.clone());
            Ok(inventory)
        })
    }

    async fn finish_session(&self, request: Request<SessionCommand>) -> Result<Response<SessionInventory>, Status> {
        self.call(request, |identity, runtime, command| {
            if command.identity.as_ref() != Some(&identity) {
                return Err(Status::failed_precondition("session runtime identity"));
            }
            let session = command.session.ok_or_else(|| Status::invalid_argument("session"))?;
            let inventory = runtime.sessions.get_mut(&session.id).ok_or_else(|| Status::not_found("session"))?;
            inventory.phase = SessionPhase::Ended as i32;
            Ok(inventory.clone())
        })
    }
}

#[tonic::async_trait]
impl Gameplay for Runtimes {
    async fn configuration(
        &self,
        request: Request<ConfigurationRequest>,
    ) -> Result<Response<ConfigurationResponse>, Status> {
        self.call(request, |identity, _, requested| {
            if requested.deployment != identity.deployment {
                return Err(Status::failed_precondition("deployment"));
            }
            Ok(ConfigurationResponse {
                deployment: identity.deployment,
                process_generation: identity.generation,
                protocol: 776,
                runtime_id: identity.runtime_id,
            })
        })
    }

    async fn prepare_player(&self, request: Request<PlayerDelivery>) -> Result<Response<PlayerPreparation>, Status> {
        self.call(request, |identity, runtime, delivery| {
            if delivery.runtime_id != identity.runtime_id || delivery.process_generation != identity.generation {
                return Err(Status::failed_precondition("delivery runtime identity"));
            }
            let operation = delivery.operation_id.clone();
            // Instant synthetic arrival; no JVM startup, player socket or world simulation.
            runtime.deliveries.insert(
                operation.clone(),
                DeliveryInventory { delivery: Some(delivery), phase: DeliveryPhase::Arrived as i32 },
            );
            Ok(PlayerPreparation { operation_id: operation, endpoint: "127.0.0.1:1".into(), capability: vec![42; 32] })
        })
    }

    async fn withdraw_player(&self, request: Request<PlayerWithdrawal>) -> Result<Response<PlayerWithdrawal>, Status> {
        self.call(request, |_, runtime, withdrawal| {
            let binding =
                runtime.deliveries.get_mut(&withdrawal.operation_id).ok_or_else(|| Status::not_found("delivery"))?;
            if binding.delivery.as_ref().is_none_or(|delivery| delivery.owner_generation != withdrawal.owner_generation)
            {
                return Err(Status::failed_precondition("delivery generation"));
            }
            binding.phase = DeliveryPhase::Closed as i32;
            Ok(withdrawal)
        })
    }
}

#[tonic::async_trait]
impl NodeControl for Runtimes {
    async fn health(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessHealth>, Status> {
        self.call(request, |identity, runtime, requested| {
            if identity != requested {
                return Err(Status::failed_precondition("health runtime identity"));
            }
            runtime.ticks += 100;
            Ok(ProcessHealth { identity: Some(identity), ready: true, tick_count: runtime.ticks, ..Default::default() })
        })
    }

    async fn stop_process(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessIdentity>, Status> {
        self.call(request, |identity, _, requested| {
            if identity != requested {
                return Err(Status::failed_precondition("stop runtime identity"));
            }
            Ok(identity)
        })
    }
}
