//! A fake JVM for control to place players on. It runs the first host control ensures, and reports each session
//! control wants as ready and each player it prepares as arrived.

use super::JVM;
use chunk_control::{Progress, RuntimeConnection};
use chunk_proto::v1::{
    ClaimRequest, ConfigurationRequest, ConfigurationResponse, DeliveryInventory, DeliveryPhase, DeploymentRef,
    Identity, PlayerDelivery, PlayerPreparation, PlayerWithdrawal, ProcessHealth, ProcessIdentity, ProcessReport,
    SessionDemand, SessionInventory, SessionMethodPhase, SessionMethodRequest, SessionMethodResult, SessionPhase,
    gameplay_server::{Gameplay, GameplayServer},
    node_control_server::{NodeControl, NodeControlServer},
    session_methods_server::{SessionMethods, SessionMethodsServer},
    supervisor_client::SupervisorClient,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, OnceLock},
};
use tokio::{sync::mpsc, task::JoinHandle};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tonic::{Request, Response, Status, transport::server::TcpIncoming};

pub const PLAYER: &str = "00000000-0000-0000-0000-000000000001";

pub fn release() -> chunk_control::Release {
    serde_json::from_value(serde_json::json!({
        "apps": {"bridge": {"id": "bridge", "jar": "bridge.jar", "sha256": "digest", "java_version": 25,
            "sessions": {"default": {"machine_profile": "small", "capacity": 8}}}},
        "deployment": {"environment": "test", "deployment": "test"}, "artifact_digest": "digest",
        "profiles": {"small": {"memory_mib": 512, "max_sessions": 2}},
        "session_types": {"bridge/default": {"app": "bridge", "machine_profile": "small", "capacity": 8}},
        "max_processes": 1, "idle_node_timeout_seconds": 0, "session_methods": session_methods()
    }))
    .unwrap()
}

/// Declares `status`, which the fake JVM answers with 7, or with `limit` 0 holds queued until it's cancelled. Its
/// optional `pad` only adds size.
pub fn session_methods() -> serde_json::Value {
    serde_json::json!({"version": 1, "methods": [{
        "app": "bridge", "session": "default", "name": "status",
        "arguments": {"type": "object", "fields": {
            "limit": {"schema": {"type": "integer"}}, "pad": {"schema": {"type": "string"}, "optional": true}
        }},
        "result": {"type": "integer"}
    }]})
}

pub fn demand(key: &str) -> SessionDemand {
    SessionDemand { key: key.into(), session_type: "bridge/default".into(), machine_profile: "small".into() }
}

pub fn login() -> ClaimRequest {
    ClaimRequest {
        operation_id: "login".into(),
        proxy_id: "proxy".into(),
        connection_id: "connection".into(),
        identity: Some(Identity { uuid: PLAYER.into(), username: "player".into(), properties: vec![] }),
        demand: Some(demand("lobby")),
        source: None,
        deployment: String::new(),
    }
}

fn identity(id: &str) -> ProcessIdentity {
    ProcessIdentity {
        deployment: Some(DeploymentRef { environment: "test".into(), deployment: "test".into() }),
        runtime_id: id.into(),
        process_id: format!("jvm-{id}"),
        generation: 1,
        machine_profile: "small".into(),
        artifact_digest: "digest".into(),
        app_id: "bridge".into(),
    }
}

#[derive(Default)]
struct State {
    host: Option<String>,
    released: BTreeSet<String>,
    deliveries: BTreeMap<String, DeliveryInventory>,
    ticks: u64,
    reports: Option<mpsc::UnboundedSender<ProcessReport>>,
    /// Session methods run, held queued, and cancelled.
    methods: (usize, usize, usize),
}

#[derive(Clone)]
pub struct Runtime {
    endpoint: String,
    control: Arc<OnceLock<String>>,
    state: Arc<Mutex<State>>,
}

impl Runtime {
    /// Serves the JVM's RPCs until the returned task is aborted.
    pub fn start() -> (Self, JoinHandle<()>) {
        let incoming = TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let endpoint = format!("http://{}", incoming.local_addr().unwrap());
        let runtime = Self { endpoint, control: Arc::default(), state: Arc::default() };
        let server = tonic::transport::Server::builder()
            .add_service(GameplayServer::new(runtime.clone()))
            .add_service(NodeControlServer::new(runtime.clone()))
            .add_service(SessionMethodsServer::new(runtime.clone()))
            .serve_with_incoming(incoming);
        (runtime, tokio::spawn(async { server.await.unwrap() }))
    }

    /// How many session methods ran, were held queued, and were cancelled.
    pub fn methods(&self) -> (usize, usize, usize) {
        self.state.lock().unwrap().methods
    }

    pub fn host(&self) -> Option<String> {
        self.state.lock().unwrap().host.clone()
    }

    fn identity(&self) -> ProcessIdentity {
        identity(&self.host().unwrap_or_default())
    }

    /// Reports to control, first opening the supervisor stream that follows the sessions control wants.
    fn report(&self, sessions: Vec<SessionInventory>, deliveries: Vec<DeliveryInventory>) {
        let report = ProcessReport { identity: Some(self.identity()), sessions, deliveries };
        let mut state = self.state.lock().unwrap();
        let reports = state.reports.get_or_insert_with(|| {
            let (reports, receiver) = mpsc::unbounded_channel();
            tokio::spawn(self.clone().follow(receiver));
            reports
        });
        let _ = reports.send(report);
    }

    async fn follow(self, reports: mpsc::UnboundedReceiver<ProcessReport>) {
        let mut request = Request::new(UnboundedReceiverStream::new(reports));
        request.metadata_mut().insert("authorization", format!("Bearer {JVM}").parse().unwrap());
        let endpoint = self.control.get().cloned().unwrap_or_default();
        let Ok(mut client) = SupervisorClient::connect(endpoint).await else {
            return;
        };
        let Ok(desired) = client.sync(request).await else {
            return;
        };
        let mut desired = desired.into_inner();
        while let Ok(Some(desired)) = desired.message().await {
            let created = desired.create.into_iter().map(|command| (command, SessionPhase::Ready));
            let finished = desired.finish.into_iter().map(|command| (command, SessionPhase::Ended));
            let sessions = created
                .chain(finished)
                .map(|(command, phase)| SessionInventory {
                    session: command.session,
                    generation: command.generation,
                    session_type: command.session_type,
                    capacity: command.capacity,
                    phase: phase.into(),
                    ..SessionInventory::default()
                })
                .collect();
            self.report(sessions, Vec::new());
        }
    }
}

#[tonic::async_trait]
impl chunk_control::Host for Runtime {
    async fn ensure(&self, id: &str, _: &chunk_control::Release, _: &str, _: &str) -> chunk_control::Result<Progress> {
        let mut state = self.state.lock().unwrap();
        if state.released.contains(id) {
            return Ok(Progress::Failed("released".into()));
        }
        state.host.get_or_insert_with(|| id.into());
        drop(state);
        Ok(Progress::Ready(Box::new(self.connection(id).expect("a connection"))))
    }
    async fn release(&self, id: &str) -> chunk_control::Result<bool> {
        self.state.lock().unwrap().released.insert(id.into());
        Ok(true)
    }
    fn stopped(&self, id: &str) -> bool {
        self.state.lock().unwrap().released.contains(id)
    }
    fn configure(&self, endpoint: String) -> chunk_control::Result<()> {
        let _ = self.control.set(endpoint);
        Ok(())
    }
    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        Some(RuntimeConnection {
            endpoint: self.endpoint.clone(),
            token: JVM.into(),
            player_endpoint: "127.0.0.1:1".into(),
            identity: identity(id),
        })
    }
    fn authenticate(&self, credential: &str) -> Option<String> {
        let state = self.state.lock().unwrap();
        state.host.clone().filter(|host| credential == JVM && !state.released.contains(host))
    }
}

#[tonic::async_trait]
impl Gameplay for Runtime {
    async fn configuration(&self, _: Request<ConfigurationRequest>) -> Result<Response<ConfigurationResponse>, Status> {
        let identity = self.identity();
        self.report(Vec::new(), Vec::new());
        Ok(Response::new(ConfigurationResponse {
            deployment: identity.deployment,
            process_generation: identity.generation,
            protocol: 776,
            runtime_id: identity.runtime_id,
        }))
    }

    async fn prepare_player(&self, request: Request<PlayerDelivery>) -> Result<Response<PlayerPreparation>, Status> {
        let delivery = request.into_inner();
        let operation = delivery.operation_id.clone();
        let inventory = DeliveryInventory { delivery: Some(delivery), phase: DeliveryPhase::Arrived.into() };
        self.state.lock().unwrap().deliveries.insert(operation.clone(), inventory.clone());
        self.report(Vec::new(), vec![inventory]);
        Ok(Response::new(PlayerPreparation {
            operation_id: operation,
            endpoint: "127.0.0.1:1".into(),
            capability: vec![42; 32],
        }))
    }

    async fn withdraw_player(&self, request: Request<PlayerWithdrawal>) -> Result<Response<PlayerWithdrawal>, Status> {
        let withdrawal = request.into_inner();
        let closed = {
            let mut state = self.state.lock().unwrap();
            let binding = state.deliveries.get_mut(&withdrawal.operation_id);
            let binding = binding.ok_or_else(|| Status::not_found("delivery"))?;
            binding.phase = DeliveryPhase::Closed.into();
            binding.clone()
        };
        self.report(Vec::new(), vec![closed]);
        Ok(Response::new(withdrawal))
    }
}

#[tonic::async_trait]
impl NodeControl for Runtime {
    async fn health(&self, _: Request<ProcessIdentity>) -> Result<Response<ProcessHealth>, Status> {
        let ticks = {
            let mut state = self.state.lock().unwrap();
            state.ticks += 100;
            state.ticks
        };
        Ok(Response::new(ProcessHealth {
            identity: Some(self.identity()),
            ready: true,
            tick_count: ticks,
            ..ProcessHealth::default()
        }))
    }

    async fn stop_process(&self, request: Request<ProcessIdentity>) -> Result<Response<ProcessIdentity>, Status> {
        Ok(Response::new(request.into_inner()))
    }
}

#[tonic::async_trait]
impl SessionMethods for Runtime {
    async fn call(&self, request: Request<SessionMethodRequest>) -> Result<Response<SessionMethodResult>, Status> {
        let request = request.into_inner();
        if request.arguments_json.contains("\"limit\":0") {
            self.state.lock().unwrap().methods.1 += 1;
            let phase = SessionMethodPhase::Accepted.into();
            let operation_id = request.operation_id;
            return Ok(Response::new(SessionMethodResult { operation_id, phase, ..SessionMethodResult::default() }));
        }
        self.state.lock().unwrap().methods.0 += 1;
        Ok(Response::new(SessionMethodResult {
            operation_id: request.operation_id,
            phase: SessionMethodPhase::Completed.into(),
            result_json: "7".into(),
            error: None,
        }))
    }

    async fn cancel(&self, request: Request<SessionMethodRequest>) -> Result<Response<SessionMethodResult>, Status> {
        self.state.lock().unwrap().methods.2 += 1;
        Ok(Response::new(SessionMethodResult {
            operation_id: request.into_inner().operation_id,
            phase: SessionMethodPhase::Cancelled.into(),
            ..SessionMethodResult::default()
        }))
    }
}
