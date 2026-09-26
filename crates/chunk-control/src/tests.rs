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
    DeliveryPhase, DeploymentRef, DesiredSessions, Identity, PlayerDelivery, PlayerPreparation, PlayerWithdrawal,
    ProcessIdentity, ProcessReport, SessionCommand, SessionDemand, SessionInventory, SessionPhase,
    gameplay_server::{Gameplay, GameplayServer},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tokio_stream::wrappers::TcpListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

use crate::{
    Config, Contracts, Control, Error, Host, MachineProfile, Progress, Release, Result, SessionType, state::Phase,
};

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
    failed_sessions: Mutex<BTreeSet<String>>,
    failed_creation: AtomicBool,
    lost_preparation: AtomicBool,
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

impl FakeRuntime {
    /// Everything this JVM holds, as it reports it to control.
    fn report(&self) -> ProcessReport {
        let (ended, failed) =
            (self.ended_sessions.lock().unwrap().clone(), self.failed_sessions.lock().unwrap().clone());
        let sessions = self.sessions.lock().unwrap().clone();
        let sessions = sessions.values().map(|command| {
            let id = &command.session.as_ref().unwrap().id;
            let phase = if failed.contains(id) {
                SessionPhase::Failed
            } else if ended.contains(id) {
                SessionPhase::Ended
            } else {
                SessionPhase::Ready
            };
            SessionInventory {
                session: command.session.clone(),
                generation: command.generation,
                session_type: command.session_type.clone(),
                phase: phase as i32,
                capacity: command.capacity,
                prepared: 0,
                attached: 0,
            }
        });
        let bindings = self.bindings.lock().unwrap();
        ProcessReport {
            identity: Some(self.identity.clone()),
            sessions: sessions.collect(),
            deliveries: bindings
                .values()
                .map(|b| DeliveryInventory { delivery: Some(b.delivery.clone()), phase: b.phase as i32 })
                .collect(),
        }
    }

    /// Runs and ends sessions as control desires. A failed session stays failed, even once asked to end.
    fn apply(&self, desired: DesiredSessions) {
        for command in desired.create {
            let id = command.session.as_ref().unwrap().id.clone();
            let mut sessions = self.sessions.lock().unwrap();
            if sessions.contains_key(&id) {
                continue;
            }
            if self.failed_creation.load(Ordering::Acquire) {
                self.failed_sessions.lock().unwrap().insert(id.clone());
            }
            sessions.insert(id, command);
        }
        for command in desired.finish {
            let id = command.session.as_ref().unwrap().id.clone();
            if !self.sessions.lock().unwrap().contains_key(&id)
                || !self.ended_sessions.lock().unwrap().insert(id.clone())
            {
                continue;
            }
            self.finishes.fetch_add(1, Ordering::AcqRel);
            for binding in self
                .bindings
                .lock()
                .unwrap()
                .values_mut()
                .filter(|binding| binding.delivery.session.as_ref().is_some_and(|session| session.id == id))
            {
                binding.phase = DeliveryPhase::Closed;
            }
        }
    }
}

/// Plays the JVM of every host `control` launched: attaches once its host has registered, then follows control's
/// desired changes and reports what changed in its state, until `stop`.
async fn follow(control: Arc<Control>, host: Arc<FakeHost>, stop: CancellationToken) {
    let mut positions = control.subscribe();
    let mut streams = BTreeMap::new();
    loop {
        let runtime = &host.runtime;
        let ids = host.ids.lock().unwrap().clone();
        for id in ids {
            // A forgotten host has not re-registered, and asking it for its connection has test side effects.
            if !runtime.available.load(Ordering::Acquire) || host.forgotten.load(Ordering::Acquire) || host.stopped(&id)
            {
                continue;
            }
            if !streams.contains_key(&id) {
                let report = host.report(&id);
                let Ok(stream) = control.attach(&id, "test-runtime-credential", report.clone()).await else {
                    continue;
                };
                streams.insert(id.clone(), (stream, None, report));
            }
            let (stream, sent, reported) = streams.get_mut(&id).unwrap();
            match control.desired(&id, sent) {
                Ok(Some(desired)) => runtime.apply(desired),
                Ok(None) => {}
                Err(_) => {
                    streams.remove(&id);
                    continue;
                }
            }
            let report = host.report(&id);
            let changes = ProcessReport {
                identity: report.identity.clone(),
                sessions: report.sessions.iter().filter(|s| !reported.sessions.contains(s)).cloned().collect(),
                deliveries: report.deliveries.iter().filter(|d| !reported.deliveries.contains(d)).cloned().collect(),
            };
            *reported = report;
            if !(changes.sessions.is_empty() && changes.deliveries.is_empty())
                && control.report(&id, *stream, &changes).await.is_err()
            {
                streams.remove(&id);
            }
        }
        tokio::select! {
            () = stop.cancelled() => return,
            _ = positions.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
}

/// Waits up to five seconds for `condition`.
async fn eventually(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("condition not reached");
}

#[tonic::async_trait]
impl Gameplay for RuntimeService {
    async fn configuration(
        &self,
        request: Request<ConfigurationRequest>,
    ) -> std::result::Result<Response<ConfigurationResponse>, Status> {
        self.check(&request)?;
        Ok(Response::new(ConfigurationResponse {
            // The JVM of every host serves here, each running the release control asks about.
            deployment: request.into_inner().deployment,
            process_generation: 1,
            runtime_id: self.identity.runtime_id.clone(),
            protocol: 776,
        }))
    }
    async fn prepare_player(
        &self,
        request: Request<PlayerDelivery>,
    ) -> std::result::Result<Response<PlayerPreparation>, Status> {
        self.check(&request)?;
        let delivery = request.into_inner();
        let mut bindings = self.bindings.lock().unwrap();
        // Like the JVM, an operation keeps its first delivery, even after it closes.
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
        // Like the JVM: an operation it never prepared is not found, and another generation is refused.
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
    /// Cannot confirm that a released runtime exited.
    unconfirmed: AtomicBool,
    /// Lost its process handles, as a host restarted with control does, until the JVM re-attaches.
    forgotten: AtomicBool,
    /// Runs once when a forgotten host is asked for its connection, after answering none.
    missed: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once when an adoption has published its process, before the adoption returns.
    adopted: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The release each host runs, where it differs from the runtime's.
    deployments: Mutex<BTreeMap<String, DeploymentRef>>,
}

impl FakeHost {
    /// The runtime's identity as host `id`'s JVM, which runs that host's release.
    fn identity(&self, id: &str) -> ProcessIdentity {
        let deployment = self.deployments.lock().unwrap().get(id).cloned();
        ProcessIdentity {
            deployment: deployment.or_else(|| self.runtime.identity.deployment.clone()),
            ..self.runtime.identity.clone()
        }
    }

    /// Everything host `id`'s JVM holds, as it reports it to control.
    fn report(&self, id: &str) -> ProcessReport {
        ProcessReport { identity: Some(self.identity(id)), ..self.runtime.report() }
    }
}
#[tonic::async_trait]
impl Host for FakeHost {
    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        if self.forgotten.load(Ordering::Acquire) {
            let missed = self.missed.lock().unwrap().take();
            if let Some(missed) = missed {
                missed();
            }
            return None;
        }
        Some(RuntimeConnection {
            endpoint: self.endpoint.clone(),
            player_endpoint: "127.0.0.1:1".into(),
            token: "test-runtime-credential".into(),
            identity: self.identity(id),
        })
    }

    async fn ensure(&self, id: &str, release: &Release, _: &str, _: &str) -> Result<Progress> {
        self.ids.lock().unwrap().insert(id.into());
        self.deployments.lock().unwrap().insert(id.into(), release.deployment.clone());
        if self.stopped(id) {
            return Ok(Progress::Failed("JVM exited".into()));
        }
        if self.forgotten.load(Ordering::Acquire) {
            return Ok(Progress::Pending);
        }
        Ok(Progress::Ready(Box::new(RuntimeConnection {
            endpoint: self.endpoint.clone(),
            player_endpoint: "127.0.0.1:1".into(),
            token: "test-runtime-credential".into(),
            identity: self.identity(id),
        })))
    }
    async fn release(&self, id: &str) -> Result<bool> {
        if self.unconfirmed.load(Ordering::Acquire) {
            return Ok(false);
        }
        self.terminated.lock().unwrap().insert(id.into());
        Ok(true)
    }
    fn stopped(&self, id: &str) -> bool {
        self.runtime.stopped.load(Ordering::Acquire) || self.terminated.lock().unwrap().contains(id)
    }
    fn unresolved(&self, _: &str) -> bool {
        self.forgotten.load(Ordering::Acquire)
    }
    fn unowned(&self) -> Result<BTreeSet<String>> {
        let forgotten = self.forgotten.load(Ordering::Acquire);
        let ids = self.ids.lock().unwrap().clone();
        Ok(if forgotten { ids.into_iter().filter(|id| !self.stopped(id)).collect() } else { BTreeSet::new() })
    }
    fn adopt(&self, token: &str, registration: chunk_proto::v1::ProcessRegistration) -> Result<()> {
        let process = registration.identity.map(|identity| identity.process_id);
        if token != "test-runtime-credential" || process.as_ref() != Some(&self.runtime.identity.process_id) {
            return Err(Error::Invalid("process credential does not match its launch record"));
        }
        assert!(self.forgotten.swap(false, Ordering::AcqRel));
        let adopted = self.adopted.lock().unwrap().take();
        if let Some(adopted) = adopted {
            adopted();
        }
        Ok(())
    }
}

/// Control's capacity executor, running until stopped.
struct Executor(CancellationToken, JoinHandle<()>);

impl Executor {
    fn start(control: &Arc<Control>) -> Self {
        let (control, stop) = (control.clone(), CancellationToken::new());
        let task = stop.clone();
        Self(stop, tokio::spawn(async move { control.run_capacity(&task).await }))
    }

    async fn stop(self) {
        self.0.cancel();
        self.1.await.unwrap();
    }
}

/// The environment `release` belongs to.
fn environment(release: &Release) -> Config {
    Config { environment: release.deployment.environment.clone() }
}

/// Opens control on an environment store of its own at `path`, through that store's backend, with `release` current.
fn open(path: &std::path::Path, release: Release, host: Arc<dyn Host>) -> Result<Arc<Control>> {
    let store = chunk_store::SqliteStore::open(path, &release.deployment.environment)?;
    let backend = chunk_backend::Backend::new(release.deployment.environment.clone(), Box::new(store))?;
    let control = Control::open(backend.system(), environment(&release), host, false)?;
    control.activate_release(release)?;
    Ok(control)
}

struct Fixture {
    directory: tempfile::TempDir,
    release: Release,
    runtime: Arc<FakeRuntime>,
    host: Arc<FakeHost>,
    stop: oneshot::Sender<()>,
    server: JoinHandle<()>,
    follower: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}
impl Fixture {
    async fn new() -> Self {
        let runtime = Arc::new(FakeRuntime {
            identity: ProcessIdentity {
                app_id: "bridge".into(),
                deployment: Some(DeploymentRef { environment: "test".into(), deployment: "build".into() }),
                runtime_id: "runtime".into(),
                process_id: "jvm".into(),
                generation: 1,
                machine_profile: "local".into(),
                artifact_digest: "artifact".into(),
            },
            method_requests: Mutex::default(),
            sessions: Mutex::default(),
            ended_sessions: Mutex::default(),
            failed_sessions: Mutex::default(),
            failed_creation: AtomicBool::new(false),
            lost_preparation: AtomicBool::new(false),
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
                .add_service(chunk_proto::v1::session_methods_server::SessionMethodsServer::new(service))
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
            unconfirmed: AtomicBool::new(false),
            forgotten: AtomicBool::new(false),
            missed: Mutex::default(),
            adopted: Mutex::default(),
            deployments: Mutex::default(),
        });
        let release = release();
        let follower = Mutex::default();
        Self { directory: tempfile::tempdir().unwrap(), release, runtime, host, stop, server, follower }
    }
    /// Opens control with its capacity executor and the fake JVM following it, once the previous control's JVM stream
    /// has closed. A reachable JVM re-attaches before this returns.
    async fn control(&self) -> Arc<Control> {
        self.detach().await;
        let control =
            open(&self.directory.path().join("control.sqlite"), self.release.clone(), self.host.clone()).unwrap();
        let stop = CancellationToken::new();
        let task = tokio::spawn({
            let (control, host, stop) = (control.clone(), self.host.clone(), stop.clone());
            async move {
                tokio::join!(follow(control.clone(), host, stop.clone()), control.run_capacity(&stop));
            }
        });
        *self.follower.lock().unwrap() = Some((stop, task));
        if self.runtime.available.load(Ordering::Acquire) && !self.host.forgotten.load(Ordering::Acquire) {
            self.recovered(&control).await;
        }
        control
    }
    /// Stops the capacity executor and closes the fake JVM's stream, so neither holds its control.
    async fn detach(&self) {
        let follower = self.follower.lock().unwrap().take();
        if let Some((stop, task)) = follower {
            stop.cancel();
            task.await.unwrap();
        }
    }
    /// Waits until control has reconciled every surviving JVM and admits new claims.
    async fn recovered(&self, control: &Control) {
        eventually(|| control.recovery.open().unwrap()).await;
    }
    /// Reports `operation`'s delivery arrived and waits until control records it.
    async fn arrive(&self, control: &Control, operation: &str) {
        self.runtime.bindings.lock().unwrap().get_mut(operation).unwrap().phase = DeliveryPhase::Arrived;
        eventually(|| control.state().unwrap().claims[operation].phase == Phase::Arrived).await;
    }
    async fn close(self) {
        self.detach().await;
        let _ = self.stop.send(());
        self.server.await.unwrap();
    }
}

impl Control {
    /// The claim's current assignment.
    fn inspect(&self, request: &ClaimRequest) -> Result<chunk_proto::v1::Assignment> {
        let claim = self.state()?.claims.get(&request.operation_id).cloned().ok_or(Error::Invalid("unknown claim"))?;
        claim.matches(request)?;
        Ok(prost::Message::decode(
            claim.assignment.ok_or(Error::Unresolved("claim preparation incomplete"))?.as_slice(),
        )?)
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
        deployment: String::new(),
    }
}

/// The move pending for `source`, from the snapshot its proxy's watch opens with.
async fn pending_move(control: &Control, source: &ClaimRequest) -> Option<ClaimRequest> {
    let (sender, mut updates) = tokio::sync::mpsc::channel(1);
    let watch = control.watch(source.proxy_id.clone(), sender, tokio_util::sync::CancellationToken::new());
    let update = tokio::select! {
        () = watch => panic!("watch ended"),
        update = updates.recv() => update.unwrap().unwrap(),
    };
    let claim =
        update.claims.into_iter().find(|claim| claim.claim.as_ref().unwrap().operation_id == source.operation_id);
    claim?.pending_move
}

#[tokio::test]
async fn concurrent_demand_coalesces_and_reservations_release_once() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
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
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let first = request("first", &uuid);
    let assignment = control.claim(first.clone()).await.unwrap();
    // The JVM reports the arrival, but the proxy never learns its activation succeeded.
    fixture.arrive(&control, "first").await;
    drop(control);
    let recovered = fixture.control().await;
    let current = recovered.inspect(&first).unwrap();
    assert_eq!(current.phase, ClaimPhase::Arrived as i32);
    assert_eq!(current.claim, assignment.claim);
    assert!(recovered.claim(request("competing", &uuid)).await.is_err());
    assert!(recovered.claim(request("alias", &uuid.to_uppercase())).await.is_err());
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(recovered.cancel(first.clone()).await.is_err());
    assert!(recovered.claim(request("still-competing", &uuid)).await.is_err());
    drop(recovered);
    let recovered = fixture.control().await;
    assert!(recovered.claim(request("after-restart", &uuid)).await.is_err());
    fixture.runtime.available.store(true, Ordering::Release);
    recovered.cancel(first.clone()).await.unwrap();
    fixture.recovered(&recovered).await;
    let second = request("second", &uuid);
    let next = recovered.claim(second.clone()).await.unwrap();
    let (previous, next_claim) = (assignment.claim.as_ref().unwrap(), next.claim.as_ref().unwrap());
    assert!(next_claim.membership_generation > previous.membership_generation);
    assert!(next_claim.delivery_generation > previous.delivery_generation);
    recovered.cancel(first).await.unwrap();
    assert_eq!(recovered.state().unwrap().players[&uuid].current.as_deref(), Some("second"));
    assert!(recovered.activate(ActivateClaim { claim: assignment.claim }).await.is_err());
    fixture.arrive(&recovered, "second").await;
    assert_eq!(
        recovered.activate(ActivateClaim { claim: next.claim }).await.unwrap().phase,
        ClaimPhase::Arrived as i32
    );
    assert!(matches!(
        open(&fixture.directory.path().join("control.sqlite"), fixture.release.clone(), fixture.host.clone()),
        Err(Error::Store(chunk_store::Error::WriterLocked))
    ));
    fixture.close().await;
}

#[tokio::test]
async fn duplicate_control_open_on_same_backend_is_rejected() {
    let fixture = Fixture::new().await;
    let environment = environment(&fixture.release);
    let store =
        chunk_store::SqliteStore::open(fixture.directory.path().join("control.sqlite"), &environment.environment)
            .unwrap();
    let backend = chunk_backend::Backend::new(environment.environment.clone(), Box::new(store)).unwrap();
    let first = Control::open(backend.system(), environment.clone(), fixture.host.clone(), false).unwrap();
    let second = Control::open(backend.system(), environment.clone(), fixture.host.clone(), false);
    let rejected = second.is_err();
    drop(second);
    drop(first);
    let reopened = Control::open(backend.system(), environment, fixture.host.clone(), false);
    let released = reopened.is_ok();
    drop(reopened);
    drop(backend);
    fixture.close().await;
    assert!(rejected, "second Control::open on the same backend must be rejected");
    assert!(released, "dropping the first control must release the environment");
}

#[tokio::test]
async fn expiry_releases_only_unactivated_reservations_and_confirmed_death_fences_active_players() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let waiting = request("waiting", &uuid::Uuid::new_v4().to_string());
    control.claim(waiting.clone()).await.unwrap();
    let active = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(active.clone()).await.unwrap();
    fixture.arrive(&control, "active").await;
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
    eventually(|| control.state().unwrap().claims["active"].phase == Phase::Released).await;
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(state.players.values().all(|p| p.current.is_none()));
    assert!(state.hosts.values().all(|h| h.retired));
    fixture.close().await;
}

#[tokio::test]
async fn moves_keep_membership_and_fence_unknown_source_outcomes_before_activation() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(ActivateClaim { claim: first.claim.clone() }).await.unwrap();
    let command = chunk_proto::v1::MovePlayerRequest {
        expected_source: None,
        expected_connection_id: String::new(),
        operation_id: "move".into(),
        player_id: uuid.clone(),
        demand: Some(SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() }),
    };
    let destination = control.move_player(command.clone()).unwrap();
    assert_eq!(pending_move(&control, &source).await.as_ref(), Some(&destination));
    let second = control.claim(destination.clone()).await.unwrap();
    let activation = ActivateClaim { claim: second.claim.clone() };
    assert!(control.activate(activation.clone()).await.is_err());
    assert_eq!(
        second.claim.as_ref().unwrap().membership_generation,
        first.claim.as_ref().unwrap().membership_generation
    );
    assert!(second.claim.as_ref().unwrap().delivery_generation > first.claim.as_ref().unwrap().delivery_generation);
    assert_ne!(second.delivery.as_ref().unwrap().session, first.delivery.as_ref().unwrap().session);
    assert_eq!(fixture.host.ids.lock().unwrap().len(), 1);
    let owner = control.state().unwrap().players[&uuid].clone();
    assert_eq!(owner.current.as_deref(), Some("source"));
    assert_eq!(owner.pending.as_deref(), Some("move"));
    let [listed] = control.players().unwrap().players.try_into().unwrap();
    assert!(listed.moving && listed.phase == ClaimPhase::Arrived as i32 && listed.app_id == "bridge");
    assert_eq!(listed.demand.unwrap().key, "lobby");
    assert!(
        control
            .move_player(chunk_proto::v1::MovePlayerRequest { operation_id: "competing".into(), ..command.clone() })
            .is_err()
    );
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(control.cancel(source.clone()).await.is_err());
    assert!(control.activate(activation.clone()).await.is_err());
    fixture.runtime.available.store(true, Ordering::Release);
    assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Withdrawing as i32);
    fixture.runtime.lost_withdrawal.store(true, Ordering::Release);
    assert!(control.cancel(source.clone()).await.is_err());
    assert!(control.activate(activation.clone()).await.is_err());
    drop(control);
    // The restarted control learns from the JVM's first report that the withdrawal completed.
    let control = fixture.control().await;
    fixture.recovered(&control).await;
    assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Released as i32);
    assert!(control.claim(request("new-login", &uuid)).await.is_err());
    control.activate(activation).await.unwrap();
    fixture.arrive(&control, "move").await;
    assert_eq!(control.inspect(&destination).unwrap().phase, ClaimPhase::Arrived as i32);
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
    let mut control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(ActivateClaim { claim: first.claim }).await.unwrap();
    let mut previous = None;
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
        if let Some(previous) = previous.take() {
            control.abandon_move(previous).await.unwrap();
        }
        let player = control.players().unwrap().players.remove(0);
        assert!(player.moving);
        assert!(player.last_move_failure.is_none());
        if prepare {
            control.claim(destination.clone()).await.unwrap();
        }
        let abandoned = chunk_proto::v1::AbandonMoveRequest {
            claim: Some(destination.clone()),
            reason: "destination preparation failed".into(),
        };
        if prepare {
            fixture.runtime.available.store(false, Ordering::Release);
            control.abandon_move(abandoned.clone()).await.unwrap();
            assert!(control.state().unwrap().players[&uuid].pending.is_some());
            assert!(control.state().unwrap().claims[operation].phase == Phase::Withdrawing);
            assert!(pending_move(&control, &source).await.is_none());
            assert!(control.players().unwrap().players[0].last_move_failure.is_some());
            control.reconcile_all().await.unwrap();
            assert!(control.state().unwrap().claims[operation].phase == Phase::Withdrawing);
            fixture.runtime.available.store(true, Ordering::Release);
        }
        control.abandon_move(abandoned.clone()).await.unwrap();
        control.reconcile_all().await.unwrap();
        assert!(control.state().unwrap().claims.get(operation).is_none_or(|claim| claim.phase == Phase::Released));
        let failure = control.players().unwrap().players.remove(0).last_move_failure.unwrap();
        assert_eq!(failure.destination, destination.demand);
        assert_eq!(failure.reason, abandoned.reason);
        assert!(failure.failed_at_ms > 0);
        drop(control);
        control = fixture.control().await;
        assert_eq!(control.players().unwrap().players[0].last_move_failure.as_ref(), Some(&failure));
        previous = Some(abandoned);
        assert!(control.claim(destination).await.is_err());
        assert!(pending_move(&control, &source).await.is_none());
        assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Arrived as i32);
        assert!(control.state().unwrap().players[&uuid].pending.is_none());
    }
    assert_eq!(fixture.runtime.bindings.lock().unwrap().len(), 2);
    fixture.close().await;
}

#[tokio::test]
async fn drain_retires_capacity_before_moves_and_enforces_its_durable_deadline() {
    for available in [true, false] {
        let fixture = Fixture::new().await;
        let control = fixture.control().await;
        let uuid = uuid::Uuid::new_v4().to_string();
        let source = request("source", &uuid);
        let first = control.claim(source.clone()).await.unwrap();
        fixture.arrive(&control, "source").await;
        control.activate(ActivateClaim { claim: first.claim }).await.unwrap();
        let command = chunk_proto::v1::DrainRequest {
            operation_id: "drain".into(),
            player_id: uuid.clone(),
            timeout_seconds: 10,
        };
        let drained = control.drain(command.clone()).unwrap();
        control.reconcile_all().await.unwrap();
        assert!(pending_move(&control, &source).await.is_some());
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
        let control = fixture.control().await;
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
        assert!(!control.state().unwrap().players.contains_key(&uuid));
        if !available {
            assert_eq!(*fixture.host.terminated.lock().unwrap(), BTreeSet::from([drained.host_id.clone()]));
            let other_host = &state.sessions[&state.claims["new-login"].session].host;
            assert!(!fixture.host.stopped(other_host));
            assert!(fixture.host.release(&drained.host_id).await.unwrap());
        }
        fixture.close().await;
    }
}

/// Releases `claim` and finishes its session as an expired destination would, returning its host.
async fn leave(control: &Arc<Control>, claim: ClaimRequest) -> String {
    let session = control.state().unwrap().claims[&claim.operation_id].session.clone();
    control.cancel(claim).await.unwrap();
    control
        .update(|state| {
            state.sessions.get_mut(&session).unwrap().retired = true;
            Ok(())
        })
        .unwrap();
    let host = control.state().unwrap().sessions[&session].host.clone();
    control.reconcile_all().await.unwrap();
    eventually(|| control.state().unwrap().sessions.get(&session).is_none_or(|session| session.finished)).await;
    control.reconcile_all().await.unwrap();
    assert!(!control.state().unwrap().sessions.contains_key(&session));
    host
}

#[tokio::test]
async fn idle_hosts_stop_once_their_last_session_has_finished_for_the_timeout() {
    let mut fixture = Fixture::new().await;
    fixture.release.idle_node_timeout_seconds = 1;
    let control = fixture.control().await;
    let first = request("first", &uuid::Uuid::new_v4().to_string());
    control.claim(first.clone()).await.unwrap();
    let host = leave(&control, first).await;
    let second = request("second", &uuid::Uuid::new_v4().to_string());
    control.claim(second.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    control.reconcile_all().await.unwrap();
    assert!(!fixture.host.stopped(&host));
    assert_eq!(leave(&control, second).await, host);
    assert!(!fixture.host.stopped(&host));
    tokio::time::sleep(Duration::from_millis(1100)).await;
    control.reconcile_all().await.unwrap();
    eventually(|| control.state().unwrap().released(&host)).await;
    assert!(fixture.host.stopped(&host));
    assert!(control.state().unwrap().hosts[&host].retired);
    control.reconcile_all().await.unwrap();
    assert!(control.state().unwrap().drains.is_empty());
    assert!(control.state().unwrap().hosts.is_empty());
    fixture.close().await;
}

/// Release `build` of environment `test`, running app `bridge` on one JVM.
pub(crate) fn release() -> Release {
    Release {
        contracts: Contracts::default(),
        apps: BTreeMap::from([("bridge".into(), test_app())]),
        deployment: DeploymentRef { environment: "test".into(), deployment: "build".into() },
        artifact_digest: "artifact".into(),
        profiles: BTreeMap::from([("local".into(), MachineProfile { memory_mib: 512, max_sessions: 2 })]),
        session_types: BTreeMap::from([(
            "bridge/default".into(),
            SessionType { app: "bridge".into(), machine_profile: "local".into(), capacity: 2 },
        )]),
        max_processes: 1,
        idle_node_timeout_seconds: 0,
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
    let control = fixture.control().await;
    let request = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(request.clone()).await.unwrap();
    fixture.arrive(&control, "active").await;
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
    let control = fixture.control().await;
    control.shutdown_node(&command).unwrap();
    assert_eq!(control.state().unwrap().drains["node/operator-stop"].deadline_ms, deadline);
    assert!(control.shutdown_node(&ShutdownNodeRequest { timeout_seconds: 0, ..command }).is_err());
    control.poll_health().await.unwrap();
    control.poll_health().await.unwrap();
    control.poll_health().await.unwrap();
    assert_eq!(control.nodes().unwrap().nodes[0].phase, NodePhase::Stopping as i32);
    assert!(!fixture.host.stopped(&online.host_id));
    assert!(control.state().unwrap().claims["active"].phase == Phase::Arrived);
    control.progress_drains().unwrap();
    eventually(|| control.state().unwrap().released(&online.host_id)).await;
    let service = crate::Service::new(control.clone(), "control-group-credential-with-32-characters".into()).unwrap();
    let mut retry = Request::new(request);
    retry.metadata_mut().insert("authorization", "Bearer control-group-credential-with-32-characters".parse().unwrap());
    let error = chunk_proto::v1::local_control_server::LocalControl::claim(&service, retry).await.unwrap_err();
    assert_eq!(error.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error.message(), "claim closed");
    control.reconcile_all().await.unwrap();
    assert_eq!(control.nodes().unwrap().nodes[0].phase, NodePhase::Stopped as i32);
    assert!(control.state().unwrap().claims["active"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn delayed_health_monitor_does_not_retire_a_surviving_runtime() {
    let fixture = Fixture::new().await;
    let control = fixture.control().await;
    let request = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(request).await.unwrap();
    fixture.arrive(&control, "active").await;
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
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    let membership = first.claim.as_ref().unwrap().membership_generation;
    fixture.arrive(&control, "source").await;
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
    assert!(next.claim.as_ref().unwrap().membership_generation > membership);
    assert!(!control.reconcile_departure(source).await.unwrap().departed);
    assert_eq!(control.state().unwrap().players[&uuid].current.as_deref(), Some("replacement"));
    assert!(control.reconcile_departure(replacement).await.unwrap().departed);
    fixture.close().await;
}

mod capacity;
mod creation;
mod destinations;
mod gateway;
mod launch;
mod log;
mod recovery;
mod releases;
mod retention;
mod roster;
mod server;
mod sync;
mod watch;
