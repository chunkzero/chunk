use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crate::RuntimeConnection;
use chunk_proto::v1::{
    ActivateClaim, ClaimPhase, ClaimRequest, ConfigurationRequest, ConfigurationResponse, DeliveryInventory,
    DeliveryPhase, DeploymentRef, Identity, PlayerDelivery, PlayerPreparation, PlayerWithdrawal, ProcessIdentity,
    ProcessInventory, SessionCommand, SessionDemand, SessionInventory, SessionPhase,
    gameplay_server::{Gameplay, GameplayServer},
    process_control_server::{ProcessControl, ProcessControlServer},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use crate::{Config, Control, Error, Host, MachineProfile, Result, SessionType, state::Phase};

struct Binding {
    delivery: PlayerDelivery,
    phase: DeliveryPhase,
}

mod prepared_methods;
mod session_methods;

struct FakeRuntime {
    identity: ProcessIdentity,
    method_requests: Mutex<BTreeMap<String, chunk_proto::v1::SessionMethodRequest>>,
    sessions: Mutex<BTreeMap<String, SessionCommand>>,
    ended_sessions: Mutex<BTreeSet<String>>,
    failed_creation: AtomicBool,
    lost_creation: AtomicBool,
    lost_preparation: AtomicBool,
    lost_finish: AtomicBool,
    finishes: AtomicUsize,
    bindings: Mutex<BTreeMap<String, Binding>>,
    available: AtomicBool,
    lost_reply: AtomicBool,
    lost_withdrawal: AtomicBool,
    stopped: AtomicBool,
    withdrawals: AtomicUsize,
    ticks: AtomicUsize,
    advance_ticks: AtomicBool,
}

#[derive(Clone)]
struct RuntimeService(Arc<FakeRuntime>);
impl std::ops::Deref for RuntimeService {
    type Target = FakeRuntime;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FakeRuntime {
    fn check<T>(&self, request: &Request<T>) -> std::result::Result<(), Status> {
        if request.metadata().get("authorization").and_then(|v| v.to_str().ok())
            != Some("Bearer test-runtime-credential")
        {
            return Err(Status::unauthenticated("fixture"));
        }
        if !self.available.load(Ordering::Acquire) {
            return Err(Status::unavailable("fixture outage"));
        }
        Ok(())
    }
}

#[tonic::async_trait]
impl ProcessControl for RuntimeService {
    async fn inventory(
        &self,
        request: Request<ProcessIdentity>,
    ) -> std::result::Result<Response<ProcessInventory>, Status> {
        self.check(&request)?;
        if request.get_ref() != &self.identity {
            return Err(Status::failed_precondition("identity"));
        }
        if self.lost_reply.swap(false, Ordering::AcqRel) {
            return Err(Status::deadline_exceeded("lost inventory reply"));
        }
        let ended = self.ended_sessions.lock().unwrap().clone();
        Ok(Response::new(ProcessInventory {
            identity: Some(self.identity.clone()),
            tick_count: 100,
            sessions: self
                .sessions
                .lock()
                .unwrap()
                .values()
                .map(|session| {
                    let mut inventory = session_inventory(session);
                    if ended.contains(&session.session.as_ref().unwrap().id) {
                        inventory.phase = SessionPhase::Ended as i32;
                    }
                    inventory
                })
                .collect(),
            deliveries: self
                .bindings
                .lock()
                .unwrap()
                .values()
                .map(|b| DeliveryInventory { delivery: Some(b.delivery.clone()), phase: b.phase as i32 })
                .collect(),
            draining: false,
        }))
    }
    async fn create_session(
        &self,
        request: Request<SessionCommand>,
    ) -> std::result::Result<Response<SessionInventory>, Status> {
        self.check(&request)?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let command = request.into_inner();
        if self.failed_creation.load(Ordering::Acquire) {
            return Err(Status::failed_precondition("failed creation"));
        }
        let id = command.session.as_ref().unwrap().id.clone();
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(previous) = sessions.get(&id)
            && previous != &command
        {
            return Err(Status::failed_precondition("changed creation"));
        }
        sessions.insert(id, command.clone());
        if self.lost_creation.swap(false, Ordering::AcqRel) {
            return Err(Status::deadline_exceeded("lost creation reply"));
        }
        Ok(Response::new(session_inventory(&command)))
    }
    async fn finish_session(
        &self,
        request: Request<SessionCommand>,
    ) -> std::result::Result<Response<SessionInventory>, Status> {
        self.check(&request)?;
        let command = request.get_ref();
        let id = &command.session.as_ref().unwrap().id;
        if !self.sessions.lock().unwrap().contains_key(id) {
            return Err(Status::not_found("unknown session"));
        }
        if self.ended_sessions.lock().unwrap().insert(id.clone()) {
            self.finishes.fetch_add(1, Ordering::AcqRel);
        }
        for binding in self
            .bindings
            .lock()
            .unwrap()
            .values_mut()
            .filter(|binding| binding.delivery.session.as_ref().is_some_and(|session| &session.id == id))
        {
            binding.phase = DeliveryPhase::Closed;
        }
        if self.lost_finish.swap(false, Ordering::AcqRel) {
            return Err(Status::deadline_exceeded("lost finish reply"));
        }
        let mut result = session_inventory(command);
        result.phase = SessionPhase::Ended as i32;
        Ok(Response::new(result))
    }
}

fn session_inventory(command: &SessionCommand) -> SessionInventory {
    SessionInventory {
        session: command.session.clone(),
        generation: command.generation,
        session_type: command.session_type.clone(),
        phase: SessionPhase::Ready as i32,
        capacity: command.capacity,
        prepared: 0,
        attached: 0,
    }
}

#[tonic::async_trait]
impl Gameplay for RuntimeService {
    async fn configuration(
        &self,
        request: Request<ConfigurationRequest>,
    ) -> std::result::Result<Response<ConfigurationResponse>, Status> {
        self.check(&request)?;
        Ok(Response::new(ConfigurationResponse {
            deployment: self.identity.deployment.clone(),
            process_generation: 1,
            runtime_id: self.identity.runtime_id.clone(),
            protocol: 775,
        }))
    }
    async fn prepare_player(
        &self,
        request: Request<PlayerDelivery>,
    ) -> std::result::Result<Response<PlayerPreparation>, Status> {
        self.check(&request)?;
        let delivery = request.into_inner();
        let mut bindings = self.bindings.lock().unwrap();
        if let Some(previous) = bindings.get(&delivery.operation_id) {
            if previous.delivery != delivery {
                return Err(Status::failed_precondition("changed preparation"));
            }
        } else {
            bindings.insert(
                delivery.operation_id.clone(),
                Binding { delivery: delivery.clone(), phase: DeliveryPhase::Prepared },
            );
        }
        if self.lost_preparation.swap(false, Ordering::AcqRel) {
            return Err(Status::deadline_exceeded("lost preparation reply"));
        }
        Ok(Response::new(PlayerPreparation {
            operation_id: delivery.operation_id,
            endpoint: "127.0.0.1:1".into(),
            capability: vec![2; 32],
        }))
    }
    async fn withdraw_player(
        &self,
        request: Request<PlayerWithdrawal>,
    ) -> std::result::Result<Response<PlayerWithdrawal>, Status> {
        self.check(&request)?;
        let withdrawal = request.into_inner();
        let mut bindings = self.bindings.lock().unwrap();
        let binding = bindings.get_mut(&withdrawal.operation_id).ok_or(Status::not_found("binding"))?;
        if binding.delivery.owner_generation != withdrawal.owner_generation {
            return Err(Status::failed_precondition("generation"));
        }
        if binding.phase != DeliveryPhase::Closed {
            self.withdrawals.fetch_add(1, Ordering::AcqRel);
            binding.phase = DeliveryPhase::Closed;
        }
        if self.lost_withdrawal.swap(false, Ordering::AcqRel) {
            return Err(Status::deadline_exceeded("lost withdrawal reply"));
        }
        Ok(Response::new(withdrawal))
    }
}

struct FakeHost {
    runtime: Arc<FakeRuntime>,
    endpoint: String,
    ids: Mutex<BTreeSet<String>>,
    terminated: Mutex<BTreeSet<String>>,
}
#[tonic::async_trait]
impl Host for FakeHost {
    fn connection(&self, _: &str) -> Option<RuntimeConnection> {
        Some(RuntimeConnection {
            endpoint: self.endpoint.clone(),
            player_endpoint: "127.0.0.1:1".into(),
            token: "test-runtime-credential".into(),
            identity: self.runtime.identity.clone(),
        })
    }

    async fn ensure(&self, id: &str, _: &str, _: &str) -> Result<RuntimeConnection> {
        self.ids.lock().unwrap().insert(id.into());
        if self.stopped(id) {
            return Err(Error::Stopped);
        }
        Ok(RuntimeConnection {
            endpoint: self.endpoint.clone(),
            player_endpoint: "127.0.0.1:1".into(),
            token: "test-runtime-credential".into(),
            identity: self.runtime.identity.clone(),
        })
    }
    async fn terminate(&self, id: &str) -> Result<()> {
        assert!(self.ids.lock().unwrap().contains(id));
        self.terminated.lock().unwrap().insert(id.into());
        Ok(())
    }
    fn stopped(&self, id: &str) -> bool {
        self.runtime.stopped.load(Ordering::Acquire) || self.terminated.lock().unwrap().contains(id)
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    config: Config,
    runtime: Arc<FakeRuntime>,
    host: Arc<FakeHost>,
    stop: oneshot::Sender<()>,
    server: JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        let deployment = DeploymentRef { environment: "test".into(), deployment: "build".into() };
        let runtime = Arc::new(FakeRuntime {
            identity: ProcessIdentity {
                app_id: "bridge".into(),
                deployment: Some(deployment.clone()),
                runtime_id: "runtime".into(),
                process_id: "jvm".into(),
                generation: 1,
                machine_profile: "local".into(),
                artifact_digest: "artifact".into(),
            },
            method_requests: Mutex::default(),
            sessions: Mutex::default(),
            ended_sessions: Mutex::default(),
            failed_creation: AtomicBool::new(false),
            lost_creation: AtomicBool::new(false),
            lost_preparation: AtomicBool::new(false),
            lost_finish: AtomicBool::new(false),
            finishes: AtomicUsize::new(0),
            bindings: Mutex::default(),
            available: AtomicBool::new(true),
            lost_reply: AtomicBool::new(false),
            lost_withdrawal: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            withdrawals: AtomicUsize::new(0),
            ticks: AtomicUsize::new(0),
            advance_ticks: AtomicBool::new(true),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let service = RuntimeService(runtime.clone());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(GameplayServer::new(service.clone()))
                .add_service(chunk_proto::v1::node_control_server::NodeControlServer::new(service.clone()))
                .add_service(chunk_proto::v1::session_methods_server::SessionMethodsServer::new(service.clone()))
                .add_service(ProcessControlServer::new(service))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = stopped.await;
                })
                .await
                .unwrap();
        });
        let host = Arc::new(FakeHost {
            runtime: runtime.clone(),
            endpoint,
            ids: Mutex::default(),
            terminated: Mutex::default(),
        });
        let config = Config {
            destinations: None,
            session_methods: None,
            session_configurations: None,
            apps: BTreeMap::from([("bridge".into(), test_app())]),
            deployment,
            artifact_digest: "artifact".into(),
            profiles: BTreeMap::from([("local".into(), MachineProfile { memory_mib: 512, max_sessions: 2 })]),
            session_types: BTreeMap::from([(
                "bridge/default".into(),
                SessionType { app: "bridge".into(), machine_profile: "local".into(), capacity: 2 },
            )]),
            max_processes: 1,
        };
        Self { directory: tempfile::tempdir().unwrap(), config, runtime, host, stop, server }
    }
    fn control(&self) -> Arc<Control> {
        Control::open(&self.directory.path().join("control.sqlite"), self.config.clone(), self.host.clone()).unwrap()
    }
    async fn close(self) {
        let _ = self.stop.send(());
        self.server.await.unwrap();
    }
}

fn request(operation: &str, player: &str) -> ClaimRequest {
    ClaimRequest {
        operation_id: operation.into(),
        proxy_id: "proxy-1".into(),
        connection_id: format!("connection-{operation}"),
        identity: Some(Identity { uuid: player.into(), username: "player".into(), properties: Vec::new() }),
        demand: Some(SessionDemand {
            key: "lobby".into(),
            session_type: "bridge/default".into(),
            machine_profile: "local".into(),
        }),
        source: None,
    }
}

#[tokio::test]
async fn concurrent_demand_coalesces_and_reservations_release_once() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let requests: Vec<_> = (0..4).map(|i| request(&format!("claim-{i}"), &uuid::Uuid::new_v4().to_string())).collect();
    let mut tasks = tokio::task::JoinSet::new();
    for request in &requests {
        let control = control.clone();
        let request = request.clone();
        tasks.spawn(async move { control.claim(request).await.unwrap() });
    }
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap().phase, ClaimPhase::Reserved as i32);
    }
    assert_eq!(fixture.runtime.sessions.lock().unwrap().len(), 2);
    assert_eq!(fixture.host.ids.lock().unwrap().len(), 1);
    assert_eq!(fixture.runtime.bindings.lock().unwrap().len(), 4);
    let original = control.claim(requests[0].clone()).await.unwrap();
    assert_eq!(original, control.claim(requests[0].clone()).await.unwrap());
    assert!(matches!(control.claim(request("full", &uuid::Uuid::new_v4().to_string())).await, Err(Error::Capacity)));
    assert_eq!(control.state().unwrap().claims.len(), 4);
    control.cancel(requests[0].clone()).await.unwrap();
    control.cancel(requests[0].clone()).await.unwrap();
    assert_eq!(fixture.runtime.withdrawals.load(Ordering::Acquire), 1);
    assert!(control.activate(ActivateClaim { claim: original.claim }).await.is_err());
    let replacement = control.claim(request("replacement", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    assert_eq!(replacement.phase, ClaimPhase::Reserved as i32);
    assert_eq!(fixture.runtime.sessions.lock().unwrap().len(), 2);
    assert_eq!(control.state().unwrap().claims.values().filter(|c| c.phase != Phase::Released).count(), 4);
    fixture.close().await;
}

#[tokio::test]
async fn recovery_reconciles_lost_activation_and_retains_unreachable_ownership() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let uuid = uuid::Uuid::new_v4().to_string();
    let first = request("first", &uuid);
    let assignment = control.claim(first.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("first").unwrap().phase = DeliveryPhase::Arrived;
    fixture.runtime.lost_reply.store(true, Ordering::Release);
    assert!(control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.is_err());
    drop(control);
    let recovered = fixture.control();
    let current = recovered.inspect(first.clone()).await.unwrap();
    assert_eq!(current.phase, ClaimPhase::Arrived as i32);
    assert_eq!(current.claim, assignment.claim);
    assert!(recovered.claim(request("competing", &uuid)).await.is_err());
    assert!(recovered.claim(request("alias", &uuid.to_uppercase())).await.is_err());
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(recovered.cancel(first.clone()).await.is_err());
    assert!(recovered.claim(request("still-competing", &uuid)).await.is_err());
    drop(recovered);
    let recovered = fixture.control();
    assert!(recovered.claim(request("after-restart", &uuid)).await.is_err());
    fixture.runtime.available.store(true, Ordering::Release);
    recovered.cancel(first.clone()).await.unwrap();
    let second = request("second", &uuid);
    let next = recovered.claim(second.clone()).await.unwrap();
    assert_eq!(next.claim.as_ref().unwrap().membership_generation, 2);
    assert_eq!(next.claim.as_ref().unwrap().delivery_generation, 2);
    recovered.cancel(first).await.unwrap();
    assert_eq!(recovered.state().unwrap().players[&uuid].current.as_deref(), Some("second"));
    assert!(recovered.activate(ActivateClaim { claim: assignment.claim }).await.is_err());
    fixture.runtime.bindings.lock().unwrap().get_mut("second").unwrap().phase = DeliveryPhase::Arrived;
    assert_eq!(
        recovered.activate(ActivateClaim { claim: next.claim }).await.unwrap().phase,
        ClaimPhase::Arrived as i32
    );
    assert!(matches!(
        Control::open(&fixture.directory.path().join("control.sqlite"), fixture.config.clone(), fixture.host.clone()),
        Err(Error::Storage(chunk_store::Error::WriterLocked))
    ));
    fixture.close().await;
}

#[tokio::test]
async fn expiry_releases_only_unactivated_reservations_and_confirmed_death_fences_active_players() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let waiting = request("waiting", &uuid::Uuid::new_v4().to_string());
    control.claim(waiting.clone()).await.unwrap();
    let active = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(active.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("active").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
    control
        .update(|state| {
            for claim in state.claims.values_mut() {
                claim.created_at_ms = 0;
            }
            Ok(())
        })
        .unwrap();
    control.reconcile_all().await.unwrap();
    assert!(control.state().unwrap().claims["waiting"].phase == Phase::Released);
    assert!(control.state().unwrap().claims["active"].phase == Phase::Arrived);
    fixture.runtime.stopped.store(true, Ordering::Release);
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.claims["active"].phase == Phase::Released);
    assert!(state.players.values().all(|p| p.current.is_none()));
    assert!(state.hosts.values().all(|h| h.retired));
    fixture.close().await;
}

#[tokio::test]
async fn moves_keep_membership_and_fence_unknown_source_outcomes_before_activation() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: first.claim.clone() }).await.unwrap();
    let command = chunk_proto::v1::MovePlayerRequest {
        expected_source: None,
        expected_connection_id: String::new(),
        operation_id: "move".into(),
        player_id: uuid.clone(),
        demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
    };
    let destination = control.move_player(command.clone()).unwrap();
    assert_eq!(control.poll_move(&source).unwrap().claim.as_ref(), Some(&destination));
    let second = control.claim(destination.clone()).await.unwrap();
    let activation = ActivateClaim { claim: second.claim.clone() };
    assert!(control.activate(activation.clone()).await.is_err());
    assert_eq!(
        second.claim.as_ref().unwrap().membership_generation,
        first.claim.as_ref().unwrap().membership_generation
    );
    assert_eq!(second.claim.as_ref().unwrap().delivery_generation, 2);
    assert_ne!(second.delivery.as_ref().unwrap().session, first.delivery.as_ref().unwrap().session);
    assert_eq!(fixture.host.ids.lock().unwrap().len(), 1);
    let owner = control.state().unwrap().players[&uuid].clone();
    assert_eq!(owner.current.as_deref(), Some("source"));
    assert_eq!(owner.pending.as_deref(), Some("move"));
    assert!(
        control
            .move_player(chunk_proto::v1::MovePlayerRequest { operation_id: "competing".into(), ..command.clone() })
            .is_err()
    );
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(control.cancel(source.clone()).await.is_err());
    assert!(control.activate(activation.clone()).await.is_err());
    fixture.runtime.available.store(true, Ordering::Release);
    assert_eq!(control.inspect(source.clone()).await.unwrap().phase, ClaimPhase::Withdrawing as i32);
    fixture.runtime.lost_withdrawal.store(true, Ordering::Release);
    assert!(control.cancel(source.clone()).await.is_err());
    assert!(control.activate(activation.clone()).await.is_err());
    drop(control);
    let control = fixture.control();
    assert_eq!(control.inspect(source.clone()).await.unwrap().phase, ClaimPhase::Released as i32);
    assert!(control.claim(request("new-login", &uuid)).await.is_err());
    fixture.runtime.lost_reply.store(true, Ordering::Release);
    assert!(control.activate(activation).await.is_err());
    fixture.runtime.bindings.lock().unwrap().get_mut("move").unwrap().phase = DeliveryPhase::Arrived;
    assert_eq!(control.inspect(destination.clone()).await.unwrap().phase, ClaimPhase::Arrived as i32);
    control.cancel(source).await.unwrap();
    let owner = control.state().unwrap().players[&uuid].clone();
    assert_eq!(owner.current.as_deref(), Some("move"));
    assert!(owner.pending.is_none());
    assert_eq!(control.move_player(command).unwrap(), destination);
    assert_eq!(fixture.runtime.bindings.lock().unwrap().len(), 2);
    fixture.close().await;
}

#[tokio::test]
async fn canceling_moves_before_preparation_or_cutover_leaves_source_usable() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: first.claim }).await.unwrap();
    for (operation, prepare) in [("queued", false), ("prepared", true)] {
        let destination = control
            .move_player(chunk_proto::v1::MovePlayerRequest {
                expected_source: None,
                expected_connection_id: String::new(),
                operation_id: operation.into(),
                player_id: uuid.clone(),
                demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
            })
            .unwrap();
        if prepare {
            control.claim(destination.clone()).await.unwrap();
        }
        control.cancel(destination.clone()).await.unwrap();
        control.cancel(destination.clone()).await.unwrap();
        assert!(control.claim(destination).await.is_err());
        assert!(control.poll_move(&source).unwrap().claim.is_none());
        assert_eq!(control.inspect(source.clone()).await.unwrap().phase, ClaimPhase::Arrived as i32);
        assert!(control.state().unwrap().players[&uuid].pending.is_none());
    }
    assert_eq!(fixture.runtime.bindings.lock().unwrap().len(), 2);
    fixture.close().await;
}

#[tokio::test]
async fn drain_retires_capacity_before_moves_and_enforces_its_durable_deadline() {
    for available in [true, false] {
        let fixture = Fixture::new().await;
        let control = fixture.control();
        let uuid = uuid::Uuid::new_v4().to_string();
        let source = request("source", &uuid);
        let first = control.claim(source.clone()).await.unwrap();
        fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
        control.activate(ActivateClaim { claim: first.claim }).await.unwrap();
        let command = chunk_proto::v1::DrainRequest {
            operation_id: "drain".into(),
            player_id: uuid.clone(),
            timeout_seconds: 10,
        };
        let drained = control.drain(command.clone()).unwrap();
        control.reconcile_all().await.unwrap();
        assert!(control.poll_move(&source).unwrap().claim.is_some());
        assert!(!fixture.runtime.stopped.load(Ordering::Acquire));
        control.claim(request("new-login", &uuid::Uuid::new_v4().to_string())).await.unwrap();
        let state = control.state().unwrap();
        assert_ne!(state.sessions[&state.claims["new-login"].session].host, drained.host_id);
        assert_eq!(control.drain(command.clone()).unwrap().deadline_ms, drained.deadline_ms);
        fixture.runtime.available.store(available, Ordering::Release);
        control.reconcile_all().await.unwrap();
        assert!(!fixture.host.stopped(&drained.host_id));
        assert_eq!(control.state().unwrap().players[&uuid].current.as_deref(), Some("source"));
        assert_eq!(fixture.runtime.bindings.lock().unwrap()["source"].phase, DeliveryPhase::Arrived);
        control
            .update(|state| {
                state.drains.get_mut("drain").unwrap().deadline_ms = 0;
                Ok(())
            })
            .unwrap();
        drop(control);
        let control = fixture.control();
        let operation = control.operation("source").unwrap();
        let guard = operation.lock().await;
        let reconciler = control.clone();
        let reconciliation = tokio::spawn(async move { reconciler.reconcile_all().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.host.stopped(&drained.host_id) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain shutdown must not wait for a busy claim");
        drop(guard);
        reconciliation.await.unwrap().unwrap();
        control.reconcile_all().await.unwrap();
        let status = control.drain(command).unwrap();
        assert!(status.stopped);
        assert_eq!(status.remaining_claims, 0);
        assert!(control.state().unwrap().players[&uuid].current.is_none());
        if !available {
            assert_eq!(*fixture.host.terminated.lock().unwrap(), BTreeSet::from([drained.host_id.clone()]));
            let other_host = &state.sessions[&state.claims["new-login"].session].host;
            assert!(!fixture.host.stopped(other_host));
            fixture.host.terminate(&drained.host_id).await.unwrap();
        }
        fixture.close().await;
    }
}

pub(crate) fn test_app() -> chunk_contract::AppArtifact {
    serde_json::from_value(serde_json::json!({"id":"bridge","jar":"app.jar","sha256":"artifact","java_version":25,
        "sessions":{"default":{"machine_profile":"local","capacity":2}}}))
    .unwrap()
}

#[tonic::async_trait]
impl chunk_proto::v1::node_control_server::NodeControl for RuntimeService {
    async fn health(
        &self,
        request: Request<ProcessIdentity>,
    ) -> std::result::Result<Response<chunk_proto::v1::ProcessHealth>, Status> {
        self.check(&request)?;
        Ok(Response::new(chunk_proto::v1::ProcessHealth {
            identity: Some(self.identity.clone()),
            ready: true,
            tick_count: if self.advance_ticks.load(Ordering::Acquire) {
                self.ticks.fetch_add(1, Ordering::AcqRel) as u64 + 1
            } else {
                self.ticks.load(Ordering::Acquire) as u64
            },
            ..Default::default()
        }))
    }
    async fn stop_process(
        &self,
        request: Request<ProcessIdentity>,
    ) -> std::result::Result<Response<ProcessIdentity>, Status> {
        self.check(&request)?;
        self.stopped.store(true, Ordering::Release);
        Ok(Response::new(self.identity.clone()))
    }
}

#[tokio::test]
async fn node_health_and_shutdown_preserve_ownership_until_confirmed_exit() {
    use chunk_proto::v1::{NodePhase, ShutdownNodeRequest};
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let request = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(request.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("active").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
    control.poll_health().await.unwrap();
    let online = control.nodes().unwrap().nodes.remove(0);
    assert_eq!(online.phase, NodePhase::Online as i32);
    assert!(online.health.as_ref().unwrap().ready);
    fixture.runtime.available.store(false, Ordering::Release);
    control.poll_health().await.unwrap();
    let unreachable = control.nodes().unwrap().nodes.remove(0);
    assert_eq!(unreachable.phase, NodePhase::Unreachable as i32);
    assert_eq!(unreachable.observed_at_ms, online.observed_at_ms);
    assert!(control.state().unwrap().claims["active"].phase == Phase::Arrived);
    let command = ShutdownNodeRequest {
        operation_id: "operator-stop".into(),
        host_id: online.host_id.clone(),
        timeout_seconds: 60,
    };
    assert_eq!(control.shutdown_node(&command).unwrap().phase, NodePhase::Draining as i32);
    let deadline = control.state().unwrap().drains["node/operator-stop"].deadline_ms;
    drop(control);
    let control = fixture.control();
    control.shutdown_node(&command).unwrap();
    assert_eq!(control.state().unwrap().drains["node/operator-stop"].deadline_ms, deadline);
    assert!(control.shutdown_node(&ShutdownNodeRequest { timeout_seconds: 0, ..command }).is_err());
    control.poll_health().await.unwrap();
    control.poll_health().await.unwrap();
    control.poll_health().await.unwrap();
    assert_eq!(control.nodes().unwrap().nodes[0].phase, NodePhase::Stopping as i32);
    assert!(!fixture.host.stopped(&online.host_id));
    assert!(control.state().unwrap().claims["active"].phase == Phase::Arrived);
    control.progress_drains().await.unwrap();
    control.reconcile_all().await.unwrap();
    assert_eq!(control.nodes().unwrap().nodes[0].phase, NodePhase::Stopped as i32);
    assert!(control.state().unwrap().claims["active"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn delayed_health_monitor_does_not_retire_a_surviving_runtime() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let request = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(request).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("active").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
    fixture.runtime.advance_ticks.store(false, Ordering::Release);
    fixture.runtime.ticks.store(1, Ordering::Release);
    let stop = tokio_util::sync::CancellationToken::new();
    let monitor = crate::server::monitor_health(&control, &stop);
    tokio::pin!(monitor);
    let observed = async {
        while control.nodes().unwrap().nodes[0].health.is_none() {
            tokio::task::yield_now().await;
        }
    };
    tokio::select! {
        () = &mut monitor => panic!("health monitor stopped"),
        result = tokio::time::timeout(Duration::from_secs(3), observed) => result.unwrap(),
    }

    // The JVM advances while control is paused, then several rapid probes see the same tick.
    tokio::time::pause();
    fixture.runtime.ticks.store(2, Ordering::Release);
    tokio::time::advance(Duration::from_secs(60)).await;
    tokio::time::resume();
    tokio::select! {
        () = &mut monitor => panic!("health monitor stopped"),
        () = tokio::time::sleep(Duration::from_secs(1)) => {},
    }
    stop.cancel();
    monitor.await;
    let node = control.nodes().unwrap().nodes.remove(0);
    assert_eq!(node.health.unwrap().tick_count, 2);
    assert_eq!(node.phase, chunk_proto::v1::NodePhase::Online as i32);
    assert_eq!(node.consecutive_failures, 0);
    let state = control.state().unwrap();
    assert!(state.drains.is_empty());
    assert!(state.hosts.values().all(|host| !host.retired));
    assert!(state.claims["active"].phase == Phase::Arrived);
    fixture.close().await;
}

#[tokio::test]
async fn departure_fences_only_the_captured_membership_and_waits_for_pending_moves() {
    let fixture = Fixture::new().await;
    let control = fixture.control();
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.runtime.bindings.lock().unwrap().get_mut("source").unwrap().phase = DeliveryPhase::Arrived;
    control.activate(ActivateClaim { claim: first.claim }).await.unwrap();
    let destination = control
        .move_player(chunk_proto::v1::MovePlayerRequest {
            expected_source: None,
            expected_connection_id: String::new(),
            operation_id: "move".into(),
            player_id: uuid.clone(),
            demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
        })
        .unwrap();
    control.claim(destination.clone()).await.unwrap();
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(control.reconcile_departure(source.clone()).await.is_err());
    fixture.runtime.available.store(true, Ordering::Release);
    assert!(!control.reconcile_departure(source.clone()).await.unwrap().departed);
    assert!(control.reconcile_departure(destination).await.unwrap().departed);
    let replacement = request("replacement", &uuid);
    let next = control.claim(replacement.clone()).await.unwrap();
    assert_eq!(next.claim.as_ref().unwrap().membership_generation, 2);
    assert!(!control.reconcile_departure(source).await.unwrap().departed);
    assert_eq!(control.state().unwrap().players[&uuid].current.as_deref(), Some("replacement"));
    assert!(control.reconcile_departure(replacement).await.unwrap().departed);
    fixture.close().await;
}

mod creation;
mod destinations;
