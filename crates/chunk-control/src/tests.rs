use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use chunk_proto::v1::{
    ActivateClaim, ClaimPhase, ClaimRequest, ConfigurationRequest, ConfigurationResponse, DeliveryInventory,
    DeliveryPhase, DeploymentRef, Identity, PlayerDelivery, PlayerPreparation, PlayerWithdrawal, ProcessIdentity,
    ProcessInventory, SessionCommand, SessionDemand, SessionInventory, SessionPhase,
    gameplay_server::{Gameplay, GameplayServer},
    process_control_server::{ProcessControl, ProcessControlServer},
};
use chunk_runtime::RuntimeConnection;
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::{Request, Response, Status};

use crate::{Config, Control, Error, Host, MachineProfile, Result, SessionType, state::Phase};

struct Binding {
    delivery: PlayerDelivery,
    phase: DeliveryPhase,
}

struct FakeRuntime {
    identity: ProcessIdentity,
    sessions: Mutex<BTreeMap<String, SessionCommand>>,
    bindings: Mutex<BTreeMap<String, Binding>>,
    available: AtomicBool,
    lost_reply: AtomicBool,
    lost_withdrawal: AtomicBool,
    stopped: AtomicBool,
    withdrawals: AtomicUsize,
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
        Ok(Response::new(ProcessInventory {
            identity: Some(self.identity.clone()),
            tick_count: 100,
            sessions: self.sessions.lock().unwrap().values().map(session_inventory).collect(),
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
    async fn stop_process(
        &self,
        request: Request<ProcessIdentity>,
    ) -> std::result::Result<Response<ProcessIdentity>, Status> {
        self.check(&request)?;
        self.stopped.store(true, Ordering::Release);
        Ok(Response::new(self.identity.clone()))
    }
    async fn create_session(
        &self,
        request: Request<SessionCommand>,
    ) -> std::result::Result<Response<SessionInventory>, Status> {
        self.check(&request)?;
        tokio::time::sleep(Duration::from_millis(30)).await;
        let command = request.into_inner();
        let id = command.session.as_ref().unwrap().id.clone();
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(previous) = sessions.get(&id)
            && previous != &command
        {
            return Err(Status::failed_precondition("changed creation"));
        }
        sessions.insert(id, command.clone());
        Ok(Response::new(session_inventory(&command)))
    }
    async fn finish_session(
        &self,
        request: Request<SessionCommand>,
    ) -> std::result::Result<Response<SessionInventory>, Status> {
        self.check(&request)?;
        let mut result = session_inventory(request.get_ref());
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
    async fn ensure(&self, id: &str, _: &str) -> Result<RuntimeConnection> {
        self.ids.lock().unwrap().insert(id.into());
        if self.stopped(id) {
            return Err(Error::Stopped);
        }
        Ok(RuntimeConnection {
            endpoint: self.endpoint.clone(),
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
                deployment: Some(deployment.clone()),
                runtime_id: "runtime".into(),
                process_id: "jvm".into(),
                generation: 1,
                machine_profile: "local".into(),
                artifact_digest: "artifact".into(),
            },
            sessions: Mutex::default(),
            bindings: Mutex::default(),
            available: AtomicBool::new(true),
            lost_reply: AtomicBool::new(false),
            lost_withdrawal: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            withdrawals: AtomicUsize::new(0),
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = oneshot::channel();
        let service = RuntimeService(runtime.clone());
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(GameplayServer::new(service.clone()))
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
            deployment,
            artifact_digest: "artifact".into(),
            profiles: BTreeMap::from([("local".into(), MachineProfile { memory_mib: 512, max_sessions: 2 })]),
            session_types: BTreeMap::from([(
                "bridge".into(),
                SessionType { machine_profile: "local".into(), capacity: 2 },
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
            session_type: "bridge".into(),
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
