use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex, atomic::Ordering},
    time::Duration,
};

use chunk_proto::{
    control::v1::{ClaimPhase, ClaimRequest, DeploymentRef, Identity, SessionDemand},
    sync::v1::{self as wire, JvmDeliveryPhase},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    Config, Contracts, Control, Error, Generation, Host, MachineProfile, MoveRequest, MoveSource, Release, Result,
    SessionType,
    gateway::{Delta, View},
    state::Phase,
};
use jvm::{CREDENTIAL, FakeHost, FakeRuntime, follow};

mod jvm;
mod session_methods;

/// The operator's `players` view: each player with a current claim, by UUID.
fn players(control: &Arc<Control>) -> Vec<(String, wire::OperatorPlayer)> {
    use prost::Message;
    let (_, snapshot) = crate::operator::Players::open(control).unwrap();
    let entries = snapshot.upserts.into_iter();
    entries
        .map(|entry| match entry.state {
            Some(wire::entry::State::Value(value)) => (entry.key, wire::OperatorPlayer::decode(&value[..]).unwrap()),
            _ => panic!("a snapshot holds values"),
        })
        .collect()
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
    control.activate_release(release, crate::DrainPolicy::default())?;
    Ok(control)
}

struct Fixture {
    directory: tempfile::TempDir,
    release: Release,
    runtime: Arc<FakeRuntime>,
    host: Arc<FakeHost>,
    follower: Mutex<Option<(CancellationToken, JoinHandle<()>)>>,
}
impl Fixture {
    fn new() -> Self {
        let runtime = Arc::new(FakeRuntime::new(crate::JvmIdentity {
            host: "runtime".into(),
            process_id: "jvm".into(),
            generation: 1,
            deployment: "build".into(),
            app: "bridge".into(),
            profile: "local".into(),
            artifact_digest: "artifact".into(),
        }));
        let host = Arc::new(FakeHost::new(runtime.clone()));
        let follower = Mutex::default();
        Self { directory: tempfile::tempdir().unwrap(), release: release(), runtime, host, follower }
    }
    /// Opens control with its capacity executor and the fake JVM following it, once the previous control's JVM streams
    /// have closed. A reachable JVM re-attaches before this returns.
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
    /// Stops the capacity executor and closes the fake JVM's streams, so neither holds its control.
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
        self.runtime.bindings.lock().unwrap().get_mut(operation).unwrap().phase = JvmDeliveryPhase::Arrived;
        eventually(|| control.state().unwrap().claims[operation].phase == Phase::Arrived).await;
    }
    async fn close(self) {
        self.detach().await;
    }
}

impl Control {
    /// The claim's current assignment.
    fn inspect(&self, request: &ClaimRequest) -> Result<chunk_proto::control::v1::Assignment> {
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
        decline_reconnect: false,
    }
}

/// The move pending for `source`, from the snapshot its gateway's topic opens with.
fn pending_move(control: &Control, source: &ClaimRequest) -> Option<ClaimRequest> {
    let (_, snapshot) = View::open(control, &source.proxy_id, None).unwrap();
    let (_, claim) = snapshot.upserts.into_iter().find(|(operation, _)| *operation == source.operation_id)?;
    claim.pending_move
}

/// Waits up to a second for a commit that changes `view`'s claims, and returns that change as the gateway's topic
/// sends it.
async fn changed(
    control: &Control,
    view: &mut View,
    positions: &mut tokio::sync::watch::Receiver<Generation>,
) -> Delta {
    let changed = async {
        loop {
            positions.changed().await.unwrap();
            let delta = view.next(control).unwrap();
            if let Some(delta) = delta.filter(|delta| !delta.upserts.is_empty() || !delta.removed.is_empty()) {
                return delta;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(1), changed).await.expect("a prompt change")
}

#[tokio::test]
async fn concurrent_demand_coalesces_and_reservations_release_once() {
    let fixture = Fixture::new();
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
    assert_eq!(control.logins(), 4);
    assert!(matches!(control.claim(request("full", &uuid::Uuid::new_v4().to_string())).await, Err(Error::Capacity)));
    assert_eq!(control.state().unwrap().claims.len(), 4);
    control.cancel(requests[0].clone()).await.unwrap();
    control.cancel(requests[0].clone()).await.unwrap();
    assert_eq!(fixture.runtime.withdrawals.load(Ordering::Acquire), 1);
    assert!(control.activate(original.claim.unwrap()).await.is_err());
    let replacement = control.claim(request("replacement", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    assert_eq!(replacement.phase, ClaimPhase::Reserved as i32);
    // A player leaving doesn't lower the count.
    assert_eq!(control.logins(), 5);
    assert_eq!(fixture.runtime.sessions.lock().unwrap().len(), 2);
    assert_eq!(control.state().unwrap().claims.values().filter(|c| c.phase != Phase::Released).count(), 4);
    fixture.close().await;
}

#[tokio::test]
async fn recovery_reconciles_lost_activation_and_retains_unreachable_ownership() {
    let fixture = Fixture::new();
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
    assert!(recovered.activate(assignment.claim.unwrap()).await.is_err());
    fixture.arrive(&recovered, "second").await;
    assert_eq!(recovered.activate(next.claim.unwrap()).await.unwrap().phase, ClaimPhase::Arrived as i32);
    assert!(matches!(
        open(&fixture.directory.path().join("control.sqlite"), fixture.release.clone(), fixture.host.clone()),
        Err(Error::Store(chunk_store::Error::WriterLocked))
    ));
    fixture.close().await;
}

#[tokio::test]
async fn duplicate_control_open_on_same_backend_is_rejected() {
    let fixture = Fixture::new();
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
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let waiting = request("waiting", &uuid::Uuid::new_v4().to_string());
    control.claim(waiting.clone()).await.unwrap();
    let active = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(active.clone()).await.unwrap();
    fixture.arrive(&control, "active").await;
    control.activate(assignment.claim.unwrap()).await.unwrap();
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
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(first.claim.clone().unwrap()).await.unwrap();
    let command = MoveRequest {
        operation_id: "move".into(),
        player_id: uuid.clone(),
        demand: SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() },
        source: None,
    };
    let destination = control.move_player(command.clone()).unwrap();
    assert_eq!(pending_move(&control, &source).as_ref(), Some(&destination));
    let second = control.claim(destination.clone()).await.unwrap();
    assert_eq!(control.logins(), 1, "a move is no login");
    let activation = second.claim.clone().unwrap();
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
    let [(_, listed)] = players(&control).try_into().unwrap();
    assert!(listed.moving && listed.phase() == wire::ClaimPhase::Arrived && listed.app == "bridge");
    assert_eq!(listed.demand.unwrap().key, "lobby");
    assert!(control.move_player(MoveRequest { operation_id: "competing".into(), ..command.clone() }).is_err());
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(control.cancel(source.clone()).await.is_err());
    assert!(control.activate(activation.clone()).await.is_err());
    assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Withdrawing as i32);
    drop(control);
    fixture.runtime.available.store(true, Ordering::Release);
    // The restarted control learns from the JVM's first report that the withdrawal completed.
    let control = fixture.control().await;
    assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Released as i32);
    let between = control.move_player(MoveRequest { operation_id: "between".into(), ..command.clone() });
    assert!(matches!(between, Err(Error::Refused(chunk_contract::MoveRefusal::Stale))), "{between:?}");
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
    let fixture = Fixture::new();
    let mut control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, "source").await;
    control.activate(first.claim.unwrap()).await.unwrap();
    let mut previous = None;
    for (operation, prepare) in [("queued", false), ("prepared", true)] {
        let destination = control
            .move_player(MoveRequest {
                operation_id: operation.into(),
                player_id: uuid.clone(),
                demand: SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() },
                source: None,
            })
            .unwrap();
        if let Some((claim, reason)) = previous.take() {
            control.abandon_move(claim, reason).await.unwrap();
        }
        let (_, player) = players(&control).remove(0);
        assert!(player.moving);
        assert!(player.last_move_failure.is_none());
        if prepare {
            control.claim(destination.clone()).await.unwrap();
        }
        let reason = String::from("destination preparation failed");
        if prepare {
            fixture.runtime.available.store(false, Ordering::Release);
            control.abandon_move(destination.clone(), reason.clone()).await.unwrap();
            assert!(control.state().unwrap().players[&uuid].pending.is_some());
            assert!(control.state().unwrap().claims[operation].phase == Phase::Withdrawing);
            assert!(pending_move(&control, &source).is_none());
            assert!(players(&control)[0].1.last_move_failure.is_some());
            control.reconcile_all().await.unwrap();
            assert!(control.state().unwrap().claims[operation].phase == Phase::Withdrawing);
            fixture.runtime.available.store(true, Ordering::Release);
        }
        control.abandon_move(destination.clone(), reason.clone()).await.unwrap();
        control.reconcile_all().await.unwrap();
        assert!(control.state().unwrap().claims.get(operation).is_none_or(|claim| claim.phase == Phase::Released));
        let failure = players(&control).remove(0).1.last_move_failure.unwrap();
        assert_eq!(failure.destination.as_ref().map(|demand| demand.key.as_str()), Some("arena"));
        assert_eq!(failure.reason, reason);
        assert!(failure.failed_at_ms > 0);
        drop(control);
        control = fixture.control().await;
        assert_eq!(players(&control)[0].1.last_move_failure.as_ref(), Some(&failure));
        previous = Some((destination.clone(), reason));
        assert!(control.claim(destination).await.is_err());
        assert!(pending_move(&control, &source).is_none());
        assert_eq!(control.inspect(&source).unwrap().phase, ClaimPhase::Arrived as i32);
        assert!(control.state().unwrap().players[&uuid].pending.is_none());
    }
    assert_eq!(fixture.runtime.bindings.lock().unwrap().len(), 2);
    fixture.close().await;
}

#[tokio::test]
async fn drain_retires_capacity_before_moves_and_enforces_its_durable_deadline() {
    for available in [true, false] {
        let fixture = Fixture::new();
        let control = fixture.control().await;
        let uuid = uuid::Uuid::new_v4().to_string();
        let source = request("source", &uuid);
        let first = control.claim(source.clone()).await.unwrap();
        fixture.arrive(&control, "source").await;
        control.activate(first.claim.unwrap()).await.unwrap();
        let command = wire::DrainArguments {
            target: Some(wire::drain_arguments::Target::Player(uuid.clone())),
            timeout_seconds: 10,
        };
        let drained = control.drain_operator("drain", &command).unwrap();
        control.reconcile_all().await.unwrap();
        assert!(pending_move(&control, &source).is_some());
        assert!(!fixture.runtime.stopped.load(Ordering::Acquire));
        control.claim(request("new-login", &uuid::Uuid::new_v4().to_string())).await.unwrap();
        let state = control.state().unwrap();
        assert_ne!(state.sessions[&state.claims["new-login"].session].host, drained.host);
        assert_eq!(control.drain_operator("drain", &command).unwrap(), drained);
        fixture.runtime.available.store(available, Ordering::Release);
        control.reconcile_all().await.unwrap();
        assert!(!fixture.host.stopped(&drained.host));
        assert_eq!(control.state().unwrap().players[&uuid].current.as_deref(), Some("source"));
        assert_eq!(fixture.runtime.bindings.lock().unwrap()["source"].phase, JvmDeliveryPhase::Arrived);
        control
            .update(|state| {
                state.drains.get_mut("operator/drain").unwrap().deadline_ms = 0;
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
            while !fixture.host.stopped(&drained.host) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain shutdown must not wait for a busy claim");
        drop(guard);
        reconciliation.await.unwrap().unwrap();
        control.reconcile_all().await.unwrap();
        assert_eq!(control.drain_operator("drain", &command).unwrap().host, drained.host);
        assert!(control.state().unwrap().released(&drained.host));
        assert!(!control.state().unwrap().players.contains_key(&uuid));
        if !available {
            assert_eq!(*fixture.host.terminated.lock().unwrap(), BTreeSet::from([drained.host.clone()]));
            let other_host = &state.sessions[&state.claims["new-login"].session].host;
            assert!(!fixture.host.stopped(other_host));
            assert!(fixture.host.release(&drained.host).await.unwrap());
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
    let mut fixture = Fixture::new();
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
        release_id: "artifact".into(),
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

#[tokio::test]
async fn node_health_and_shutdown_preserve_ownership_until_confirmed_exit() {
    use chunk_proto::{control::v1::ShutdownNodeRequest, sync::v1::NodePhase};
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let request = request("active", &uuid::Uuid::new_v4().to_string());
    let assignment = control.claim(request.clone()).await.unwrap();
    fixture.arrive(&control, "active").await;
    control.activate(assignment.claim.unwrap()).await.unwrap();
    control.poll_health().unwrap();
    let online = control.nodes().unwrap().remove(0);
    assert_eq!(online.phase, NodePhase::Online);
    assert!(online.health.as_ref().unwrap().ready);
    fixture.runtime.unhealthy.store(true, Ordering::Release);
    eventually(|| control.jvms.health(&online.host).is_some_and(|health| !health.ready)).await;
    control.poll_health().unwrap();
    let unhealthy = control.nodes().unwrap().remove(0);
    assert_eq!((unhealthy.phase, unhealthy.consecutive_failures), (NodePhase::Unhealthy, 1));
    assert!(control.state().unwrap().claims["active"].phase == Phase::Arrived);
    let command =
        ShutdownNodeRequest { operation_id: "operator-stop".into(), host_id: online.host.clone(), timeout_seconds: 60 };
    control.shutdown_node(&command).unwrap();
    assert_eq!(control.nodes().unwrap()[0].phase, NodePhase::Draining);
    let deadline = control.state().unwrap().drains["node/operator-stop"].deadline_ms;
    drop(control);
    let control = fixture.control().await;
    control.shutdown_node(&command).unwrap();
    assert_eq!(control.state().unwrap().drains["node/operator-stop"].deadline_ms, deadline);
    assert!(control.shutdown_node(&ShutdownNodeRequest { timeout_seconds: 0, ..command }).is_err());
    control.poll_health().unwrap();
    control.poll_health().unwrap();
    control.poll_health().unwrap();
    assert_eq!(control.nodes().unwrap()[0].phase, NodePhase::Stopping);
    assert!(!fixture.host.stopped(&online.host));
    assert!(control.state().unwrap().claims["active"].phase == Phase::Arrived);
    control.progress_drains().unwrap();
    eventually(|| control.state().unwrap().released(&online.host)).await;
    let closed = control.claim(request).await.unwrap_err();
    assert!(matches!(closed, Error::Invalid("claim closed")), "{closed}");
    control.reconcile_all().await.unwrap();
    assert_eq!(control.nodes().unwrap()[0].phase, NodePhase::Stopped);
    assert!(control.state().unwrap().claims["active"].phase == Phase::Released);
    fixture.close().await;
}

#[tokio::test]
async fn captured_moves_reject_replaced_connections_even_for_existing_operations() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("move-source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    fixture.arrive(&control, &source.operation_id).await;
    control.activate(first.claim.clone().unwrap()).await.unwrap();
    let captured = MoveSource { claim: first.claim.unwrap(), connection_id: source.connection_id.clone() };
    let expected = MoveRequest {
        operation_id: "captured-move".into(),
        player_id: uuid.clone(),
        demand: source.demand.clone().unwrap(),
        source: Some(captured.clone()),
    };
    for connection_id in ["", "replacement"] {
        let source = MoveSource { connection_id: connection_id.into(), ..captured.clone() };
        assert!(control.move_player(MoveRequest { source: Some(source), ..expected.clone() }).is_err());
    }
    let queued = control.move_player(expected.clone()).unwrap();
    assert_eq!(control.move_player(expected.clone()).unwrap(), queued);
    control.cancel(queued).await.unwrap();
    control.cancel(source).await.unwrap();
    let replacement = request("replacement", &uuid);
    let next = control.claim(replacement.clone()).await.unwrap();
    fixture.arrive(&control, &replacement.operation_id).await;
    control.activate(next.claim.clone().unwrap()).await.unwrap();
    assert!(control.move_player(expected.clone()).is_err());
    assert!(
        control
            .move_player(MoveRequest {
                source: Some(MoveSource {
                    claim: next.claim.unwrap(),
                    connection_id: replacement.connection_id.clone()
                }),
                ..expected.clone()
            })
            .is_err()
    );
    assert!(control.move_player(MoveRequest { operation_id: "new-stale-move".into(), ..expected }).is_err());
    assert!(pending_move(&control, &replacement).is_none());
    fixture.close().await;
}

#[tokio::test]
async fn departure_fences_only_the_captured_membership_and_waits_for_pending_moves() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let uuid = uuid::Uuid::new_v4().to_string();
    let source = request("source", &uuid);
    let first = control.claim(source.clone()).await.unwrap();
    let membership = first.claim.as_ref().unwrap().membership_generation;
    fixture.arrive(&control, "source").await;
    control.activate(first.claim.unwrap()).await.unwrap();
    let destination = control
        .move_player(MoveRequest {
            operation_id: "move".into(),
            player_id: uuid.clone(),
            demand: SessionDemand { key: "arena".into(), ..source.demand.clone().unwrap() },
            source: None,
        })
        .unwrap();
    control.claim(destination.clone()).await.unwrap();
    fixture.runtime.available.store(false, Ordering::Release);
    assert!(control.reconcile_departure(source.clone()).await.is_err());
    fixture.runtime.available.store(true, Ordering::Release);
    assert!(!control.reconcile_departure(source.clone()).await.unwrap());
    assert!(control.reconcile_departure(destination).await.unwrap());
    let replacement = request("replacement", &uuid);
    let next = control.claim(replacement.clone()).await.unwrap();
    assert!(next.claim.as_ref().unwrap().membership_generation > membership);
    assert!(!control.reconcile_departure(source).await.unwrap());
    assert_eq!(control.state().unwrap().players[&uuid].current.as_deref(), Some("replacement"));
    assert!(control.reconcile_departure(replacement).await.unwrap());
    fixture.close().await;
}

#[test]
fn control_refuses_a_short_persisted_credential() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("token");
    assert_eq!(crate::server::credential(&path).unwrap().len(), 64);
    for short in ["", "x"] {
        std::fs::write(&path, short).unwrap();
        assert!(crate::server::credential(&path).is_err());
    }
}

mod capacity;
mod creation;
mod destinations;
mod draining;
mod gateway;
mod launch;
mod log;
mod moves;
mod recovery;
mod releases;
mod retention;
mod roster;
mod sync;
mod watch;
