//! The fake JVM: one runtime playing the JVM of every host control launches. It registers each host's JVM, follows
//! the host's `jvm/<host>` topic and reports what changed, as a JVM does over the sync protocol.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use chunk_proto::sync::v1::{
    self as sync, JvmDelivery, JvmDeliveryPhase, JvmDeliveryStatus, JvmHealth, JvmMethodCall, JvmMethodPhase,
    JvmMethodResult, JvmRegistration, JvmReport, JvmSession, JvmSessionPhase, JvmSessionStatus,
};
use prost::Message;
use tokio_util::sync::CancellationToken;

use crate::{
    Control, Error, Host, JvmIdentity, Progress, Registration, Release, Result, RuntimeConnection, jvm::Topic,
};

/// The credential of every host's JVM.
pub(super) const CREDENTIAL: &str = "test-runtime-credential";

/// A delivery the fake JVM holds, into `session` at its claim's `generation`.
#[derive(Clone)]
pub(super) struct Binding {
    pub(super) session: String,
    pub(super) generation: Option<sync::Position>,
    pub(super) phase: JvmDeliveryPhase,
}

pub(super) struct FakeRuntime {
    pub(super) identity: JvmIdentity,
    /// The session methods it ran, by operation.
    pub(super) method_requests: Mutex<BTreeMap<String, JvmMethodCall>>,
    pub(super) sessions: Mutex<BTreeMap<String, JvmSession>>,
    pub(super) ended_sessions: Mutex<BTreeSet<String>>,
    pub(super) failed_sessions: Mutex<BTreeSet<String>>,
    pub(super) failed_creation: AtomicBool,
    pub(super) finishes: AtomicUsize,
    /// Every delivery it holds, by operation. Like the JVM, an operation keeps its first delivery, even after it closes.
    pub(super) bindings: Mutex<BTreeMap<String, Binding>>,
    /// Follows its topics and reports while set.
    pub(super) available: AtomicBool,
    /// Holds back reporting the deliveries it prepared while set.
    pub(super) stalled_preparation: AtomicBool,
    /// Keeps each withdrawn delivery open while set.
    pub(super) stalled_withdrawal: AtomicBool,
    /// Reports itself not ready while set.
    pub(super) unhealthy: AtomicBool,
    pub(super) stopped: AtomicBool,
    pub(super) withdrawals: AtomicUsize,
}

impl FakeRuntime {
    pub(super) fn new(identity: JvmIdentity) -> Self {
        Self {
            identity,
            method_requests: Mutex::default(),
            sessions: Mutex::default(),
            ended_sessions: Mutex::default(),
            failed_sessions: Mutex::default(),
            failed_creation: AtomicBool::new(false),
            finishes: AtomicUsize::new(0),
            bindings: Mutex::default(),
            available: AtomicBool::new(true),
            stalled_preparation: AtomicBool::new(false),
            stalled_withdrawal: AtomicBool::new(false),
            unhealthy: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            withdrawals: AtomicUsize::new(0),
        }
    }

    /// Runs and ends sessions, and prepares and closes deliveries, as `host`'s topic asks, closing each delivery on the
    /// host it leaves out. Returns whether the topic asks the JVM to stop, and the methods it carries.
    fn apply(&self, fake: &FakeHost, host: &str, update: &sync::Update) -> (bool, Vec<(String, JvmMethodCall)>) {
        let entries: BTreeMap<_, _> = update
            .upserts
            .iter()
            .filter_map(|entry| match &entry.state {
                Some(sync::entry::State::Value(value)) => Some((entry.key.as_str(), &value[..])),
                _ => None,
            })
            .collect();
        let mut methods = Vec::new();
        for (key, value) in &entries {
            if let Some(id) = key.strip_prefix("session/") {
                self.session(fake, host, id, JvmSession::decode(*value).unwrap());
            } else if let Some(operation) = key.strip_prefix("delivery/") {
                self.delivery(operation, &JvmDelivery::decode(*value).unwrap());
            } else if let Some(operation) = key.strip_prefix("method/") {
                methods.push((operation.into(), JvmMethodCall::decode(*value).unwrap()));
            }
        }
        let holds = fake.holds(host);
        for (operation, binding) in self.bindings.lock().unwrap().iter_mut() {
            if holds(&binding.session) && !entries.contains_key(format!("delivery/{operation}").as_str()) {
                binding.phase = JvmDeliveryPhase::Closed;
            }
        }
        (entries.contains_key("stop"), methods)
    }

    /// Runs session `id` on `host`, or ends it and closes its deliveries. A failed session stays failed, even once
    /// asked to end.
    fn session(&self, fake: &FakeHost, host: &str, id: &str, wanted: JvmSession) {
        if !wanted.finish {
            fake.sessions.lock().unwrap().insert(id.into(), host.into());
            let mut sessions = self.sessions.lock().unwrap();
            if !sessions.contains_key(id) {
                if self.failed_creation.load(Ordering::Acquire) {
                    self.failed_sessions.lock().unwrap().insert(id.into());
                }
                sessions.insert(id.into(), wanted);
            }
            return;
        }
        if !self.sessions.lock().unwrap().contains_key(id) || !self.ended_sessions.lock().unwrap().insert(id.into()) {
            return;
        }
        self.finishes.fetch_add(1, Ordering::AcqRel);
        for binding in self.bindings.lock().unwrap().values_mut().filter(|binding| binding.session == id) {
            binding.phase = JvmDeliveryPhase::Closed;
        }
    }

    /// Prepares `operation`'s delivery, or closes it once withdrawn.
    fn delivery(&self, operation: &str, wanted: &JvmDelivery) {
        let mut bindings = self.bindings.lock().unwrap();
        let binding = bindings.entry(operation.into()).or_insert_with(|| Binding {
            session: wanted.session.clone(),
            generation: wanted.generation,
            phase: if wanted.withdraw { JvmDeliveryPhase::Closed } else { JvmDeliveryPhase::Prepared },
        });
        if wanted.withdraw
            && binding.phase != JvmDeliveryPhase::Closed
            && !self.stalled_withdrawal.load(Ordering::Acquire)
        {
            binding.phase = JvmDeliveryPhase::Closed;
            self.withdrawals.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// Runs method `score` once for an arrived caller, answering 7, and never runs a method cancelled first.
    fn answer(&self, operation: &str, call: JvmMethodCall) -> JvmMethodResult {
        let mut ran = self.method_requests.lock().unwrap();
        if !ran.contains_key(operation) {
            let arrived = || {
                let bindings = self.bindings.lock().unwrap();
                bindings.get(&call.delivery).is_some_and(|binding| binding.phase == JvmDeliveryPhase::Arrived)
            };
            if call.cancel {
                return JvmMethodResult { phase: JvmMethodPhase::Cancelled.into(), ..JvmMethodResult::default() };
            }
            if call.method != "score" || !arrived() {
                return JvmMethodResult { phase: JvmMethodPhase::Failed.into(), ..JvmMethodResult::default() };
            }
            ran.insert(operation.into(), call);
        }
        JvmMethodResult { phase: JvmMethodPhase::Completed.into(), result_json: b"7".into() }
    }
}

pub(super) struct FakeHost {
    pub(super) runtime: Arc<FakeRuntime>,
    pub(super) ids: Mutex<BTreeSet<String>>,
    /// Hosts whose JVM registered.
    pub(super) registered: Mutex<BTreeSet<String>>,
    pub(super) terminated: Mutex<BTreeSet<String>>,
    /// Cannot confirm that a released runtime exited.
    pub(super) unconfirmed: AtomicBool,
    /// Keeps each JVM starting, so claims wait for it.
    pub(super) starting: AtomicBool,
    /// Lost its process handles, as a host restarted with control does, until the JVM re-attaches.
    pub(super) forgotten: AtomicBool,
    /// Runs once when a forgotten host is asked for its connection, after answering none.
    pub(super) missed: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Runs once when an adoption has published its process, before the adoption returns.
    pub(super) adopted: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The release each host runs, where it differs from the runtime's.
    pub(super) deployments: Mutex<BTreeMap<String, String>>,
    /// The host each session control asked for runs on.
    pub(super) sessions: Mutex<BTreeMap<String, String>>,
}

impl FakeHost {
    pub(super) fn new(runtime: Arc<FakeRuntime>) -> Self {
        Self {
            runtime,
            ids: Mutex::default(),
            registered: Mutex::default(),
            terminated: Mutex::default(),
            unconfirmed: AtomicBool::new(false),
            starting: AtomicBool::new(false),
            forgotten: AtomicBool::new(false),
            missed: Mutex::default(),
            adopted: Mutex::default(),
            deployments: Mutex::default(),
            sessions: Mutex::default(),
        }
    }

    /// The runtime's identity as host `id`'s JVM, which runs that host's release.
    pub(super) fn identity(&self, id: &str) -> JvmIdentity {
        let deployment = self.deployments.lock().unwrap().get(id).cloned();
        JvmIdentity {
            host: id.into(),
            deployment: deployment.unwrap_or_else(|| self.runtime.identity.deployment.clone()),
            ..self.runtime.identity.clone()
        }
    }

    /// What host `id`'s JVM registers with.
    pub(super) fn registration(&self, id: &str) -> JvmRegistration {
        let identity = self.identity(id);
        JvmRegistration {
            process_id: identity.process_id,
            generation: identity.generation,
            app: identity.app,
            profile: identity.profile,
            artifact_digest: identity.artifact_digest,
            deployment: identity.deployment,
            player_endpoint: "127.0.0.1:1".into(),
            protocol: 776,
        }
    }

    /// Whether a session runs on host `id`: one control asked no other host for.
    fn holds(&self, id: &str) -> impl Fn(&str) -> bool {
        let (sessions, id) = (self.sessions.lock().unwrap().clone(), id.to_owned());
        move |session| sessions.get(session).is_none_or(|host| *host == id)
    }

    /// Everything host `id`'s JVM holds, and its health, as it reports them.
    pub(super) fn report(&self, id: &str) -> JvmReport {
        let (holds, runtime) = (self.holds(id), &self.runtime);
        let ended = runtime.ended_sessions.lock().unwrap().clone();
        let failed = runtime.failed_sessions.lock().unwrap().clone();
        let sessions = runtime.sessions.lock().unwrap().clone();
        let sessions = sessions.into_iter().filter(|(session, _)| holds(session)).map(|(session, wanted)| {
            let phase = if failed.contains(&session) {
                JvmSessionPhase::Failed
            } else if ended.contains(&session) {
                JvmSessionPhase::Ended
            } else {
                JvmSessionPhase::Ready
            };
            JvmSessionStatus {
                id: session,
                session_type: wanted.session_type,
                capacity: wanted.capacity,
                phase: phase.into(),
                ..JvmSessionStatus::default()
            }
        });
        let bindings = runtime.bindings.lock().unwrap().clone();
        let stalled = runtime.stalled_preparation.load(Ordering::Acquire);
        let reported = |binding: &Binding| !(stalled && binding.phase == JvmDeliveryPhase::Prepared);
        let bindings = bindings.into_iter().filter(|(_, binding)| holds(&binding.session) && reported(binding));
        let deliveries = bindings.map(|(operation, binding)| {
            let prepared = binding.phase == JvmDeliveryPhase::Prepared;
            JvmDeliveryStatus {
                operation_id: operation,
                generation: binding.generation,
                phase: binding.phase.into(),
                capability: if prepared { vec![2; 32] } else { Vec::new() },
            }
        });
        let health =
            JvmHealth { ready: !runtime.unhealthy.load(Ordering::Acquire), tick_count: 1, ..JvmHealth::default() };
        JvmReport {
            complete: false,
            sessions: sessions.collect(),
            deliveries: deliveries.collect(),
            health: Some(health),
        }
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
        self.registered.lock().unwrap().contains(id).then(|| RuntimeConnection {
            player_endpoint: "127.0.0.1:1".into(),
            token: CREDENTIAL.into(),
            identity: self.identity(id),
        })
    }

    async fn ensure(&self, id: &str, release: &Release, _: &str, _: &str) -> Result<Progress> {
        self.ids.lock().unwrap().insert(id.into());
        self.deployments.lock().unwrap().insert(id.into(), release.deployment.deployment.clone());
        if self.stopped(id) {
            return Ok(Progress::Failed("JVM exited".into()));
        }
        if self.forgotten.load(Ordering::Acquire) || self.starting.load(Ordering::Acquire) {
            return Ok(Progress::Pending);
        }
        Ok(self.connection(id).map_or(Progress::Pending, |connection| Progress::Ready(Box::new(connection))))
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
    fn register(&self, token: &str, registration: Registration) -> Result<()> {
        let identity = registration.identity;
        if self.forgotten.load(Ordering::Acquire)
            || token != format!("Bearer {CREDENTIAL}")
            || identity != self.identity(&identity.host)
        {
            return Err(Error::Invalid("unknown process"));
        }
        self.registered.lock().unwrap().insert(identity.host);
        Ok(())
    }
    fn adopt(&self, token: &str, registration: Registration) -> Result<()> {
        let identity = registration.identity;
        if token != CREDENTIAL || identity.process_id != self.runtime.identity.process_id {
            return Err(Error::Invalid("process credential does not match its launch record"));
        }
        assert!(self.forgotten.swap(false, Ordering::AcqRel));
        self.registered.lock().unwrap().insert(identity.host);
        let adopted = self.adopted.lock().unwrap().take();
        if let Some(adopted) = adopted {
            adopted();
        }
        Ok(())
    }
}

/// Plays the JVM of every host `control` launched while the runtime is available, until `stop`: registers, follows the
/// host's topic and reports what changed. A JVM asked to stop ends its sessions, reports, and closes its stream for
/// good, leaving its deliveries to its host's confirmed exit.
pub(super) async fn follow(control: Arc<Control>, host: Arc<FakeHost>, stop: CancellationToken) {
    let mut positions = control.subscribe();
    let mut followed = BTreeMap::new();
    let mut stopped = BTreeSet::new();
    let mut streams = 0;
    loop {
        let ids = host.ids.lock().unwrap().clone();
        for id in ids {
            // A forgotten host has not re-registered, and asking it for its connection has test side effects.
            if !host.runtime.available.load(Ordering::Acquire)
                || host.forgotten.load(Ordering::Acquire)
                || host.stopped(&id)
                || stopped.contains(&id)
            {
                followed.remove(&id);
                continue;
            }
            if !followed.contains_key(&id) {
                streams += 1;
                let Some(stream) = Followed::open(&control, &host, &id, format!("stream-{streams}")) else {
                    continue;
                };
                followed.insert(id.clone(), stream);
            }
            match followed.get_mut(&id).map(|stream| stream.step(&control, &host, &id)) {
                Some(Ok(true)) => {}
                Some(Ok(false)) => {
                    followed.remove(&id);
                    stopped.insert(id);
                }
                _ => {
                    followed.remove(&id);
                }
            }
        }
        tokio::select! {
            () = stop.cancelled() => return,
            _ = positions.changed() => {}
            () = tokio::time::sleep(Duration::from_millis(10)) => {}
        }
    }
}

/// One of the fake JVM's topic streams.
struct Followed {
    topic: Topic,
    stream: String,
    /// The topic's first snapshot, until applied.
    first: Option<sync::Update>,
    /// Everything the stream reported so far.
    reported: Option<JvmReport>,
}

impl Followed {
    fn open(control: &Arc<Control>, host: &FakeHost, id: &str, stream: String) -> Option<Self> {
        control.register_jvm(id, CREDENTIAL, host.registration(id)).ok()?;
        let (topic, first) = Topic::open(control, id, &stream).ok()?;
        Some(Self { topic, stream, first: Some(first), reported: None })
    }

    /// Applies the topic's latest update, reports what changed and answers the update's methods. `false` once the
    /// topic asks the JVM to stop.
    fn step(&mut self, control: &Control, host: &FakeHost, id: &str) -> Result<bool> {
        let update = match self.first.take() {
            Some(first) => Some(first),
            None => self.topic.update()?,
        };
        let (mut stop, mut methods) = (false, Vec::new());
        if let Some(update) = update {
            (stop, methods) = host.runtime.apply(host, id, &update);
        }
        if stop {
            let holds = host.holds(id);
            let sessions = host.runtime.sessions.lock().unwrap().clone();
            host.runtime.ended_sessions.lock().unwrap().extend(sessions.into_keys().filter(|session| holds(session)));
        }
        let report = host.report(id);
        let changes = match &self.reported {
            None => JvmReport { complete: true, ..report.clone() },
            Some(previous) => JvmReport {
                complete: false,
                sessions: report.sessions.iter().filter(|s| !previous.sessions.contains(s)).cloned().collect(),
                deliveries: report.deliveries.iter().filter(|d| !previous.deliveries.contains(d)).cloned().collect(),
                health: report.health.filter(|health| previous.health.as_ref() != Some(health)),
            },
        };
        if changes.complete
            || !(changes.sessions.is_empty() && changes.deliveries.is_empty() && changes.health.is_none())
        {
            control.report_jvm(id, CREDENTIAL, &self.stream, &changes)?;
        }
        self.reported = Some(report);
        for (operation, call) in methods {
            control.method_result(id, &self.stream, &operation, host.runtime.answer(&operation, call))?;
        }
        Ok(!stop)
    }
}
