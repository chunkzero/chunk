//! Control's effects on a JVM registered over sync: placing and withdrawing through its topic, and recovering one
//! that outlived core.

use super::{
    jvm::{ACCEPTED, Launches, session, sessions},
    *,
};
use chunk_proto::{
    control::v1::Assignment,
    sync::v1::{
        ClaimPhase, JvmDelivery, JvmDeliveryPhase, JvmDeliveryStatus, JvmHealth, JvmMethodCall, JvmMethodPhase,
        JvmMethodResult, JvmReport, JvmSession, JvmSessionPhase, JvmSessionStatus, OperatorPlayer,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

const CAPABILITY: [u8; 32] = [7; 32];

/// A JVM following its topic over sync. It runs each session it is asked for, prepares each delivery and closes it
/// once withdrawn or gone, unless a test stalls withdrawals. It answers methods `score` and `status` with 7, `hold` and
/// `status` with `limit` 0 only once they are cancelled, and never answers `stuck`. Its players arrive and leave when a
/// test says. It closes its stream once asked to stop.
#[derive(Clone)]
pub(super) struct SyncJvm {
    client: CoreClient<Channel>,
    host: String,
    held: Arc<Mutex<Held>>,
}

#[derive(Default)]
struct Held {
    stream: String,
    sessions: BTreeMap<String, JvmSessionStatus>,
    deliveries: BTreeMap<String, JvmDeliveryStatus>,
    /// Methods on the topic, those ever runnable there, and those answered.
    methods: BTreeMap<String, JvmMethodCall>,
    runnable: BTreeSet<String>,
    answered: BTreeSet<String>,
    /// How many methods it completed, held until cancelled, and cancelled.
    counts: (usize, usize, usize),
    /// Keeps each withdrawn delivery open while set.
    stalled: bool,
}

/// Whether the JVM answers `call` only once it is cancelled.
fn holds(call: &JvmMethodCall) -> bool {
    call.method == "hold" || (call.method == "status" && call.arguments_json.windows(9).any(|w| w == b"\"limit\":0"))
}

impl SyncJvm {
    async fn start(fixture: &Fixture, host: &str) -> Self {
        Self::connect(fixture.client.clone(), host).await
    }

    /// Registers `host`'s JVM through `client` and follows its topic.
    pub(super) async fn connect(client: CoreClient<Channel>, host: &str) -> Self {
        let registration = CallRequest {
            method: "chunk:register".into(),
            arguments: super::jvm::registration().encode_to_vec(),
            ..CallRequest::default()
        };
        let registered = client.clone().call(authorized(registration, JVM)).await.unwrap().into_inner();
        assert!(matches!(registered.outcome, Some(Outcome::Result(_))), "{registered:?}");
        let jvm = Self { client, host: host.into(), held: Arc::default() };
        jvm.follow().await;
        jvm
    }

    /// Opens a stream, which supersedes the previous one, and follows it until it ends.
    pub(super) async fn follow(&self) {
        let subscription = SubscribeRequest { topic: format!("jvm/{}", self.host), ..SubscribeRequest::default() };
        let mut updates = self.client.clone().subscribe(authorized(subscription, JVM)).await.unwrap().into_inner();
        self.apply(next(&mut updates).await, true).await;
        let jvm = self.clone();
        tokio::spawn(async move {
            while let Ok(Some(update)) = updates.message().await {
                if update.error.is_some() {
                    return;
                }
                // Like a JVM that exits once asked to stop, it closes its stream after reporting.
                let stop = update.upserts.iter().any(|entry| entry.key == "stop");
                jvm.apply(update, false).await;
                if stop {
                    return;
                }
            }
        });
    }

    async fn apply(&self, update: Update, first: bool) {
        let (stream, report, answers) = {
            let mut held = self.held.lock().unwrap();
            if first {
                held.stream.clone_from(&update.stream);
            }
            let mut report = JvmReport { complete: first, ..JvmReport::default() };
            let mut keys = BTreeSet::new();
            for entry in &update.upserts {
                let Some(State::Value(value)) = &entry.state else { continue };
                keys.insert(entry.key.clone());
                if let Some(id) = entry.key.strip_prefix("session/") {
                    let wanted = JvmSession::decode(&value[..]).unwrap();
                    let phase = if wanted.finish { JvmSessionPhase::Ended } else { JvmSessionPhase::Ready };
                    let status = JvmSessionStatus {
                        session_type: wanted.session_type,
                        capacity: wanted.capacity,
                        ..session(id, phase)
                    };
                    if held.sessions.insert(id.into(), status.clone()).as_ref() != Some(&status) {
                        report.sessions.push(status);
                    }
                } else if let Some(operation) = entry.key.strip_prefix("delivery/") {
                    let wanted = JvmDelivery::decode(&value[..]).unwrap();
                    let current = held.deliveries.get(operation).map(JvmDeliveryStatus::phase);
                    let phase = match current {
                        _ if wanted.withdraw && !held.stalled => JvmDeliveryPhase::Closed,
                        None => JvmDeliveryPhase::Prepared,
                        Some(phase) => phase,
                    };
                    if current != Some(phase) {
                        let status = delivery(operation, wanted.generation, phase);
                        held.deliveries.insert(operation.into(), status.clone());
                        report.deliveries.push(status);
                    }
                } else if let Some(operation) = entry.key.strip_prefix("method/") {
                    let call = JvmMethodCall::decode(&value[..]).unwrap();
                    if !call.cancel && held.runnable.insert(operation.into()) && holds(&call) {
                        held.counts.1 += 1;
                    }
                    held.methods.insert(operation.into(), call);
                }
            }
            // A delivery whose key is gone closes, and is forgotten once reported closed.
            for (operation, status) in &mut held.deliveries {
                if !keys.contains(&format!("delivery/{operation}")) && status.phase() != JvmDeliveryPhase::Closed {
                    *status = delivery(operation, status.generation, JvmDeliveryPhase::Closed);
                    report.deliveries.push(status.clone());
                }
            }
            if first {
                report.sessions = held.sessions.values().cloned().collect();
                report.deliveries = held.deliveries.values().cloned().collect();
            }
            held.deliveries.retain(|operation, status| {
                keys.contains(&format!("delivery/{operation}")) || status.phase() != JvmDeliveryPhase::Closed
            });
            held.methods.retain(|operation, _| keys.contains(&format!("method/{operation}")));
            let Held { methods, answered, .. } = &mut *held;
            answered.retain(|operation| methods.contains_key(operation));
            let answers: Vec<_> = methods
                .iter()
                .filter(|(operation, _)| !answered.contains(*operation))
                .filter_map(|(operation, call)| {
                    let result = match call.method.as_str() {
                        _ if holds(call) && call.cancel => {
                            JvmMethodResult { phase: JvmMethodPhase::Cancelled.into(), ..Default::default() }
                        }
                        "score" | "status" if !holds(call) => {
                            JvmMethodResult { phase: JvmMethodPhase::Completed.into(), result_json: b"7".into() }
                        }
                        _ => return None,
                    };
                    Some((operation.clone(), result))
                })
                .collect();
            (held.stream.clone(), report, answers)
        };
        if first || !report.sessions.is_empty() || !report.deliveries.is_empty() {
            self.call("chunk:report", "", &stream, &report).await;
        }
        for (operation, result) in answers {
            if self.call("chunk:method_result", &operation, &stream, &result).await.outcome == ACCEPTED {
                let mut held = self.held.lock().unwrap();
                if held.answered.insert(operation) {
                    match result.phase() {
                        JvmMethodPhase::Completed => held.counts.0 += 1,
                        JvmMethodPhase::Cancelled => held.counts.2 += 1,
                        _ => {}
                    }
                }
            }
        }
    }

    /// Moves the player of `operation`'s delivery on, reporting it in `phase`.
    pub(super) async fn player(&self, operation: &str, phase: JvmDeliveryPhase) {
        let (stream, status) = {
            let mut held = self.held.lock().unwrap();
            let generation = held.deliveries.get(operation).expect("a held delivery").generation;
            let status = delivery(operation, generation, phase);
            held.deliveries.insert(operation.into(), status.clone());
            (held.stream.clone(), status)
        };
        let report = JvmReport { deliveries: vec![status], ..JvmReport::default() };
        assert_eq!(self.call("chunk:report", "", &stream, &report).await.outcome, ACCEPTED);
    }

    /// Keeps each delivery it is asked to withdraw open, until a test closes it.
    pub(super) fn stall_withdrawals(&self) {
        self.held.lock().unwrap().stalled = true;
    }

    /// Reports `health` on the JVM's stream.
    pub(super) async fn health(&self, health: JvmHealth) {
        let stream = self.held.lock().unwrap().stream.clone();
        let report = JvmReport { health: Some(health), ..JvmReport::default() };
        assert_eq!(self.call("chunk:report", "", &stream, &report).await.outcome, ACCEPTED);
    }

    /// How many methods the JVM completed, held until cancelled, and cancelled.
    pub(super) fn methods(&self) -> (usize, usize, usize) {
        self.held.lock().unwrap().counts
    }

    /// The operations whose deliveries the JVM holds prepared.
    pub(super) fn prepared(&self) -> Vec<String> {
        let held = self.held.lock().unwrap();
        let prepared = held.deliveries.iter().filter(|(_, status)| status.phase() == JvmDeliveryPhase::Prepared);
        prepared.map(|(operation, _)| operation.clone()).collect()
    }

    /// Whether `operation` was runnable on the topic since the JVM last forgot what was.
    pub(super) fn runnable(&self, operation: &str) -> bool {
        self.held.lock().unwrap().runnable.contains(operation)
    }

    pub(super) fn forget_runnable(&self) {
        self.held.lock().unwrap().runnable.clear();
    }

    pub(super) fn sees(&self, key: &str) -> bool {
        let held = self.held.lock().unwrap();
        match key.split_once('/') {
            Some(("delivery", operation)) => held.deliveries.contains_key(operation),
            Some(("method", operation)) => held.methods.contains_key(operation),
            _ => false,
        }
    }

    /// Waits for the JVM to forget `key`'s work once it left the topic.
    async fn forgets(&self, key: &str) {
        let forgotten = async {
            while self.sees(key) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(10), forgotten).await.expect("the JVM forgot the work");
    }

    async fn call(&self, method: &str, operation: &str, stream: &str, arguments: &impl Message) -> CallResponse {
        let message = CallRequest {
            operation_id: operation.into(),
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        self.client.clone().call(authorized(message, JVM)).await.unwrap().into_inner()
    }
}

fn delivery(operation: &str, generation: Option<Position>, phase: JvmDeliveryPhase) -> JvmDeliveryStatus {
    let capability = if phase == JvmDeliveryPhase::Prepared { CAPABILITY.to_vec() } else { Vec::new() };
    JvmDeliveryStatus { operation_id: operation.into(), generation, phase: phase.into(), capability }
}

/// The generation of `operation`'s delivery in `update`.
fn delivery_generation(update: &Update, operation: &str) -> Option<Position> {
    let entry = update.upserts.iter().find(|entry| entry.key == format!("delivery/{operation}"))?;
    let Some(State::Value(value)) = &entry.state else { return None };
    JvmDelivery::decode(&value[..]).unwrap().generation
}

/// The fake release, whose sessions declare methods `score`, `hold` and `stuck`, each taking an optional `text`.
fn release() -> chunk_control::Release {
    let mut release = runtime::release();
    let method = |name: &str| {
        serde_json::json!({"app": "bridge", "session": "default", "name": name,
            "arguments": {"type": "object", "fields": {"text": {"schema": {"type": "string"}, "optional": true}}},
            "result": {"type": "integer"}})
    };
    let methods = serde_json::json!({"version": 1, "methods": [method("score"), method("hold"), method("stuck")]});
    release.contracts.session_methods = Some(serde_json::from_value(methods).unwrap());
    release
}

/// Starts claiming the fake player, returning the claim and the host control launches for it.
async fn claiming(
    fixture: &Fixture,
    launches: &Launches,
) -> (tokio::task::JoinHandle<chunk_control::Result<Assignment>>, String) {
    fixture.control.activate_release(release(), chunk_control::DrainPolicy::default()).unwrap();
    let control = fixture.control.clone();
    let claim = tokio::spawn(async move { control.claim(runtime::login()).await });
    let host = loop {
        if let Some(host) = launches.host() {
            break host;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    (claim, host)
}

/// Places the fake player on a JVM registered over sync, returning it and the claim's assignment.
async fn place(fixture: &Fixture, launches: &Launches) -> (SyncJvm, Assignment) {
    let (claim, host) = claiming(fixture, launches).await;
    let jvm = SyncJvm::start(fixture, &host).await;
    (jvm, claim.await.unwrap().unwrap())
}

/// Places the fake player and lets them arrive.
pub(super) async fn arrive(fixture: &Fixture, launches: &Launches) -> (SyncJvm, Assignment) {
    let (jvm, assignment) = place(fixture, launches).await;
    fixture.control.activate(assignment.claim.clone().unwrap()).await.unwrap();
    jvm.player("login", JvmDeliveryPhase::Arrived).await;
    phase(fixture, Some(ClaimPhase::Arrived)).await;
    (jvm, assignment)
}

/// Waits for the fake player's claim to reach `phase`, or `None` for none.
pub(super) async fn phase(fixture: &Fixture, phase: Option<ClaimPhase>) {
    let reached = async {
        loop {
            if runtime::players(&fixture.control).first().map(OperatorPlayer::phase) == phase {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the claim's phase");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_is_prepared_on_the_topic_and_released_once_its_jvm_closes_it() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, assignment) = place(&fixture, &launches).await;
    let preparation = assignment.preparation.unwrap();
    assert_eq!((preparation.capability, preparation.endpoint.as_str()), (CAPABILITY.to_vec(), "127.0.0.1:1"));
    assert_eq!(assignment.configuration.unwrap().protocol, 776);

    fixture.control.activate(assignment.claim.unwrap()).await.unwrap();
    jvm.player("login", JvmDeliveryPhase::Arrived).await;
    phase(&fixture, Some(ClaimPhase::Arrived)).await;
    // The player leaves: the JVM closes the delivery, which releases the claim and leaves the topic.
    jvm.player("login", JvmDeliveryPhase::Closed).await;
    phase(&fixture, None).await;
    jvm.forgets("delivery/login").await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_withdrawal_reaches_a_jvm_that_reconnected_with_its_deliveries() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, _) = arrive(&fixture, &launches).await;
    // A newer stream supersedes the first; its complete report keeps the player where they were.
    jvm.follow().await;
    phase(&fixture, Some(ClaimPhase::Arrived)).await;
    let control = fixture.control.clone();
    tokio::time::timeout(Duration::from_secs(10), control.cancel(runtime::login())).await.unwrap().unwrap();
    phase(&fixture, None).await;
    jvm.forgets("delivery/login").await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_delivery_report_neither_supplies_nor_removes_a_capability() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (claim, host) = claiming(&fixture, &launches).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = loop {
        let update = next(&mut updates).await;
        if update.upserts.iter().any(|entry| entry.key == "delivery/login") {
            break update;
        }
    };
    let generation = delivery_generation(&first, "login");
    let stale = generation.map(|position| Position { revision: position.revision + 1, ..position });
    let status = |generation, phase, capability: [u8; 32]| JvmDeliveryStatus {
        capability: capability.to_vec(),
        ..delivery("login", generation, phase)
    };
    let report = |complete, deliveries| JvmReport { complete, deliveries, ..JvmReport::default() };

    // A position past the revision's 40 bits is rejected rather than read as another epoch's.
    let overflow =
        generation.map(|position| Position { epoch: position.epoch - 1, revision: position.revision + (1 << 40) });
    let invalid = report(true, vec![status(overflow, JvmDeliveryPhase::Prepared, [1; 32])]);
    assert_eq!(code(&fixture.report(&first.stream, &invalid).await), Code::Invalid);
    // Another generation's PREPARED supplies nothing, and its CLOSED removes nothing.
    let mut prepared = report(true, vec![status(stale, JvmDeliveryPhase::Prepared, [1; 32])]);
    prepared.sessions = sessions(&first)
        .into_iter()
        .map(|(id, wanted)| JvmSessionStatus {
            session_type: wanted.session_type,
            capacity: wanted.capacity,
            ..session(&id, JvmSessionPhase::Ready)
        })
        .collect();
    assert_eq!(fixture.report(&first.stream, &prepared).await.outcome, ACCEPTED);
    let current = report(
        false,
        vec![status(generation, JvmDeliveryPhase::Prepared, [2; 32]), status(stale, JvmDeliveryPhase::Closed, [0; 32])],
    );
    assert_eq!(fixture.report(&first.stream, &current).await.outcome, ACCEPTED);
    let assignment = claim.await.unwrap().unwrap();
    assert_eq!(assignment.preparation.unwrap().capability, [2; 32]);
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_survivor_serving_players_is_adopted_once_it_closes_those_the_log_lost() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    arrive(&fixture, &launches).await;
    let host = launches.host().unwrap();

    // Core restarts over a log that kept the player's claim but lost another player's the JVM also serves.
    let survivor = Launches::of(&host, true);
    let fixture = fixture.restart(Arc::new(survivor.clone())).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    let (id, _) = sessions(&first).pop_first().expect("a session");
    let keys: Vec<_> = first.upserts.iter().map(|entry| entry.key.as_str()).collect();
    assert!(keys.contains(&"delivery/login") && !keys.contains(&"delivery/lost"));
    let generation = delivery_generation(&first, "login");
    let lost = Some(Position { epoch: 1, revision: 1 });
    let report = JvmReport {
        complete: true,
        sessions: vec![JvmSessionStatus { attached: 2, ..session(&id, JvmSessionPhase::Ready) }],
        deliveries: vec![
            delivery("login", generation, JvmDeliveryPhase::Arrived),
            delivery("lost", lost, JvmDeliveryPhase::Arrived),
            delivery("closed", lost, JvmDeliveryPhase::Closed),
        ],
        health: None,
    };
    assert_eq!(fixture.report(&first.stream, &report).await.outcome, ACCEPTED);
    let claim = fixture.control.claim(runtime::login()).await;
    assert!(matches!(claim, Err(chunk_control::Error::Busy)), "{claim:?}");

    // The JVM closes the delivery its topic leaves out; admission reopens without stopping it.
    let closed =
        JvmReport { deliveries: vec![delivery("lost", lost, JvmDeliveryPhase::Closed)], ..JvmReport::default() };
    assert_eq!(fixture.report(&first.stream, &closed).await.outcome, ACCEPTED);
    let reopened = async {
        while matches!(fixture.control.claim(runtime::login()).await, Err(chunk_control::Error::Busy)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), reopened).await.expect("admission reopened");
    assert!(!survivor.0.lock().unwrap().released);
    phase(&fixture, Some(ClaimPhase::Arrived)).await;
    // A retry of the lost operation the JVM first reported already closed is rejected rather than reserved again.
    let mut retry = runtime::login();
    retry.operation_id = "closed".into();
    retry.identity.as_mut().unwrap().uuid = "00000000-0000-4000-8000-000000000002".into();
    let retried = fixture.control.claim(retry).await;
    assert!(matches!(retried, Err(chunk_control::Error::Invalid("claim operation lost in a restore"))), "{retried:?}");
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_closing_a_delivery_before_its_assignment_releases_the_reservation() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (claim, host) = claiming(&fixture, &launches).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = loop {
        let update = next(&mut updates).await;
        if delivery_generation(&update, "login").is_some() {
            break update;
        }
    };
    let generation = delivery_generation(&first, "login");
    let sessions = sessions(&first).into_iter().map(|(id, wanted)| JvmSessionStatus {
        session_type: wanted.session_type,
        capacity: wanted.capacity,
        ..session(&id, JvmSessionPhase::Ready)
    });
    let report = JvmReport {
        complete: true,
        sessions: sessions.collect(),
        deliveries: vec![delivery("login", generation, JvmDeliveryPhase::Closed)],
        health: None,
    };
    assert_eq!(fixture.report(&first.stream, &report).await.outcome, ACCEPTED);
    assert!(claim.await.unwrap().is_err());
    // The reservation is released, so the delivery leaves the topic.
    while delivery_generation(&next(&mut updates).await, "login").is_some() {}
    phase(&fixture, None).await;
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_survivors_delivery_whose_assignment_a_restore_lost_is_fenced() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    // Core restarts after reserving the claim but before recording its assignment.
    let (claim, host) = claiming(&fixture, &launches).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    while delivery_generation(&next(&mut updates).await, "login").is_none() {}
    claim.abort();
    let _ = claim.await;
    drop(updates);
    let fixture = fixture.restart(Arc::new(Launches::of(&host, true))).await;

    // The surviving JVM reports the delivery prepared at the claim's generation, which a gateway could still attach.
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    let generation = delivery_generation(&first, "login");
    assert!(generation.is_some());
    let sessions = sessions(&first).into_iter().map(|(id, wanted)| JvmSessionStatus {
        session_type: wanted.session_type,
        capacity: wanted.capacity,
        prepared: 1,
        ..session(&id, JvmSessionPhase::Ready)
    });
    let report = JvmReport {
        complete: true,
        sessions: sessions.collect(),
        deliveries: vec![delivery("login", generation, JvmDeliveryPhase::Prepared)],
        health: None,
    };
    assert_eq!(fixture.report(&first.stream, &report).await.outcome, ACCEPTED);
    // Its topic leaves the delivery out, so the JVM closes it, and admission waits until it has.
    while delivery_generation(&next(&mut updates).await, "login").is_some() {}
    let claim = fixture.control.claim(runtime::login()).await;
    assert!(matches!(claim, Err(chunk_control::Error::Busy)), "{claim:?}");
    let closed =
        JvmReport { deliveries: vec![delivery("login", generation, JvmDeliveryPhase::Closed)], ..JvmReport::default() };
    assert_eq!(fixture.report(&first.stream, &closed).await.outcome, ACCEPTED);
    // Admission reopens, and a retry of the fenced operation is rejected.
    let retried = async {
        loop {
            match fixture.control.claim(runtime::login()).await {
                Err(chunk_control::Error::Busy) => tokio::time::sleep(Duration::from_millis(50)).await,
                retried => return retried,
            }
        }
    };
    let retried = tokio::time::timeout(Duration::from_secs(10), retried).await.expect("admission reopened");
    assert!(matches!(retried, Err(chunk_control::Error::Invalid("claim operation lost in a restore"))), "{retried:?}");
    drop(updates);
    fixture.stop().await;
}
