//! Synthetic runtime RPCs live in the generator process, outside target CPU/RSS.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
};

use anyhow::Result;
use chunk_proto::v1::{
    ConfigurationRequest, ConfigurationResponse, DeliveryInventory, DeliveryPhase, DeploymentRef, DesiredSessions,
    PlayerDelivery, PlayerPreparation, PlayerWithdrawal, ProcessHealth, ProcessIdentity, ProcessReport,
    SessionInventory, SessionPhase,
    gameplay_server::{Gameplay, GameplayServer},
    node_control_server::{NodeControl, NodeControlServer},
    supervisor_client::SupervisorClient,
};
use tokio::{net::TcpListener, sync::mpsc};
use tokio_stream::{StreamExt, wrappers::TcpListenerStream};
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
    /// Reports to control over this runtime's stream.
    reports: Option<mpsc::UnboundedSender<ProcessReport>>,
}

impl Runtime {
    fn report(&self, identity: &ProcessIdentity, sessions: Vec<SessionInventory>, deliveries: Vec<DeliveryInventory>) {
        if let Some(reports) = &self.reports {
            let _ = reports.send(ProcessReport { identity: Some(identity.clone()), sessions, deliveries });
        }
    }
}

#[derive(Clone, Default)]
pub struct Runtimes {
    runtimes: Arc<Mutex<BTreeMap<String, Runtime>>>,
    control: Arc<OnceLock<String>>,
}

impl Runtimes {
    pub async fn serve(self, listener: TcpListener, stop: CancellationToken) -> Result<()> {
        tonic::transport::Server::builder()
            .add_service(GameplayServer::new(self.clone()))
            .add_service(NodeControlServer::new(self))
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), stop.cancelled_owned())
            .await?;
        Ok(())
    }

    /// Sets the control endpoint each runtime streams its state to once control first calls it.
    pub fn connect(&self, endpoint: String) {
        let _ = self.control.set(endpoint);
    }

    /// Streams `id`'s state to control and follows its desired sessions until the stream ends.
    async fn sync(self, id: String, reports: mpsc::UnboundedReceiver<ProcessReport>) -> Result<()> {
        let endpoint = self.control.get().cloned().unwrap_or_default();
        let mut request = Request::new(tokio_stream::wrappers::UnboundedReceiverStream::new(reports));
        request.metadata_mut().insert("authorization", format!("Bearer bench-{id}").parse()?);
        let mut desired = SupervisorClient::connect(endpoint).await?.sync(request).await?.into_inner();
        while let Some(update) = desired.next().await {
            self.apply(&id, update?).map_err(|error| anyhow::anyhow!("{error}"))?;
        }
        Ok(())
    }

    fn apply(&self, id: &str, desired: DesiredSessions) -> Result<(), Status> {
        let mut runtimes = self.runtimes.lock().map_err(|_| Status::internal("fixture lock poisoned"))?;
        let runtime = runtimes.entry(id.into()).or_default();
        let mut changed = Vec::new();
        for (command, phase) in desired
            .create
            .into_iter()
            .map(|command| (command, SessionPhase::Ready))
            .chain(desired.finish.into_iter().map(|command| (command, SessionPhase::Ended)))
        {
            let session = command.session.ok_or_else(|| Status::invalid_argument("session"))?;
            let inventory = SessionInventory {
                session: Some(session.clone()),
                generation: command.generation,
                session_type: command.session_type,
                capacity: command.capacity,
                phase: phase as i32,
                ..Default::default()
            };
            runtime.sessions.insert(session.id, inventory.clone());
            changed.push(inventory);
        }
        runtime.report(&identity(id), changed, Vec::new());
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
        let mut runtimes = self.runtimes.lock().map_err(|_| Status::internal("fixture lock poisoned"))?;
        let runtime = runtimes.entry(id.into()).or_default();
        if runtime.reports.is_none() && self.control.get().is_some() {
            let (reports, receiver) = mpsc::unbounded_channel();
            let _ = reports.send(ProcessReport {
                identity: Some(process.clone()),
                sessions: runtime.sessions.values().cloned().collect(),
                deliveries: runtime.deliveries.values().cloned().collect(),
            });
            runtime.reports = Some(reports);
            let (runtimes, id) = (self.clone(), id.to_owned());
            tokio::spawn(async move {
                if let Err(error) = runtimes.sync(id, receiver).await {
                    tracing::debug!(%error, "synthetic runtime stream ended");
                }
            });
        }
        f(process, runtime, request.into_inner()).map(Response::new)
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
            let inventory = DeliveryInventory { delivery: Some(delivery), phase: DeliveryPhase::Arrived as i32 };
            runtime.deliveries.insert(operation.clone(), inventory.clone());
            runtime.report(&identity, Vec::new(), vec![inventory]);
            Ok(PlayerPreparation { operation_id: operation, endpoint: "127.0.0.1:1".into(), capability: vec![42; 32] })
        })
    }

    async fn withdraw_player(&self, request: Request<PlayerWithdrawal>) -> Result<Response<PlayerWithdrawal>, Status> {
        self.call(request, |identity, runtime, withdrawal| {
            let binding =
                runtime.deliveries.get_mut(&withdrawal.operation_id).ok_or_else(|| Status::not_found("delivery"))?;
            if binding.delivery.as_ref().is_none_or(|delivery| delivery.owner_generation != withdrawal.owner_generation)
            {
                return Err(Status::failed_precondition("delivery generation"));
            }
            binding.phase = DeliveryPhase::Closed as i32;
            let closed = binding.clone();
            runtime.report(&identity, Vec::new(), vec![closed]);
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
