//! Control's effects on a JVM registered over sync: placing, withdrawing and calling through its topic.

use super::{
    jvm::{ACCEPTED, Launches, session, sessions},
    *,
};
use chunk_proto::{
    sync::v1::{
        JvmDelivery, JvmDeliveryPhase, JvmDeliveryStatus, JvmMethodCall, JvmMethodPhase, JvmMethodResult, JvmReport,
        JvmSession, JvmSessionPhase, JvmSessionStatus,
    },
    v1::{ActivateClaim, Assignment, ClaimPhase, PlayerStatus, SessionMethodPhase},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

const CAPABILITY: [u8; 32] = [7; 32];

/// A JVM following its topic over sync. It runs each session it is asked for, prepares each delivery and closes it
/// once withdrawn or gone, and answers method `score` with 7 and any other method only once it is cancelled. Its
/// players arrive and leave when a test says.
#[derive(Clone)]
struct SyncJvm {
    client: CoreClient<Channel>,
    host: String,
    held: Arc<Mutex<Held>>,
}

#[derive(Default)]
struct Held {
    stream: String,
    sessions: BTreeMap<String, JvmSessionStatus>,
    deliveries: BTreeMap<String, JvmDeliveryStatus>,
    /// Methods on the topic, and those answered.
    methods: BTreeMap<String, JvmMethodCall>,
    answered: BTreeSet<String>,
}

impl SyncJvm {
    async fn start(fixture: &Fixture, host: &str) -> Self {
        fixture.register().await;
        let jvm = Self { client: fixture.client.clone(), host: host.into(), held: Arc::default() };
        jvm.follow().await;
        jvm
    }

    /// Opens a stream, which supersedes the previous one, and follows it until it ends.
    async fn follow(&self) {
        let subscription = SubscribeRequest { topic: format!("jvm/{}", self.host), ..SubscribeRequest::default() };
        let mut updates = self.client.clone().subscribe(authorized(subscription, JVM)).await.unwrap().into_inner();
        self.apply(next(&mut updates).await, true).await;
        let jvm = self.clone();
        tokio::spawn(async move {
            while let Ok(Some(update)) = updates.message().await {
                if update.error.is_some() {
                    return;
                }
                jvm.apply(update, false).await;
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
                    let wanted = JvmSession::decode(value.as_slice()).unwrap();
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
                    let wanted = JvmDelivery::decode(value.as_slice()).unwrap();
                    let current = held.deliveries.get(operation).map(JvmDeliveryStatus::phase);
                    let phase = match current {
                        _ if wanted.withdraw => JvmDeliveryPhase::Closed,
                        None => JvmDeliveryPhase::Prepared,
                        Some(phase) => phase,
                    };
                    if current != Some(phase) {
                        let status = delivery(operation, wanted.generation, phase);
                        held.deliveries.insert(operation.into(), status.clone());
                        report.deliveries.push(status);
                    }
                } else if let Some(operation) = entry.key.strip_prefix("method/") {
                    held.methods.insert(operation.into(), JvmMethodCall::decode(value.as_slice()).unwrap());
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
                        "score" => {
                            JvmMethodResult { phase: JvmMethodPhase::Completed.into(), result_json: b"7".into() }
                        }
                        _ if call.cancel => {
                            JvmMethodResult { phase: JvmMethodPhase::Cancelled.into(), ..Default::default() }
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
                self.held.lock().unwrap().answered.insert(operation);
            }
        }
    }

    /// Moves the player of `operation`'s delivery on, reporting it in `phase`.
    async fn player(&self, operation: &str, phase: JvmDeliveryPhase) {
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

    fn sees(&self, key: &str) -> bool {
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

/// The fake release, whose sessions declare methods `score` and `hold`.
fn release() -> chunk_control::Release {
    let mut release = runtime::release();
    let method = |name: &str| {
        serde_json::json!({"app": "bridge", "session": "default", "name": name,
            "arguments": {"type": "object", "fields": {}}, "result": {"type": "integer"}})
    };
    let methods = serde_json::json!({"version": 1, "methods": [method("score"), method("hold")]});
    release.contracts.session_methods = Some(serde_json::from_value(methods).unwrap());
    release
}

/// Places the fake player on a JVM registered over sync, returning it and the claim's assignment.
async fn place(fixture: &Fixture, launches: &Launches) -> (SyncJvm, Assignment) {
    fixture.control.activate_release(release()).unwrap();
    let control = fixture.control.clone();
    let claim = tokio::spawn(async move { control.claim(runtime::login()).await });
    let host = loop {
        if let Some(host) = launches.host() {
            break host;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let jvm = SyncJvm::start(fixture, &host).await;
    (jvm, claim.await.unwrap().unwrap())
}

/// Places the fake player and lets them arrive.
async fn arrive(fixture: &Fixture, launches: &Launches) -> (SyncJvm, Assignment) {
    let (jvm, assignment) = place(fixture, launches).await;
    fixture.control.activate(ActivateClaim { claim: assignment.claim.clone() }).await.unwrap();
    jvm.player("login", JvmDeliveryPhase::Arrived).await;
    phase(fixture, Some(ClaimPhase::Arrived)).await;
    (jvm, assignment)
}

/// Waits for the fake player's claim to reach `phase`, or `None` for none.
async fn phase(fixture: &Fixture, phase: Option<ClaimPhase>) {
    let reached = async {
        loop {
            let players = fixture.control.players().unwrap().players;
            if players.first().map(PlayerStatus::phase) == phase {
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

    fixture.control.activate(ActivateClaim { claim: assignment.claim }).await.unwrap();
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
async fn session_methods_run_and_cancel_through_the_topic() {
    let launches = Launches::default();
    let fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, assignment) = arrive(&fixture, &launches).await;
    let control = fixture.control.clone();
    let captured = control.capture_session(assignment.claim.as_ref().unwrap()).unwrap();
    let timeout = Duration::from_secs(10);

    let score = control.prepare_session_method(&captured, "score", serde_json::json!({}), timeout).unwrap();
    let result = control.call_session_method(&score, &CancellationToken::new()).await.unwrap();
    assert_eq!((result.phase(), result.result_json.as_str()), (SessionMethodPhase::Completed, "7"));
    // A retry returns the recorded result rather than running the method again.
    assert_eq!(control.call_session_method(&score, &CancellationToken::new()).await.unwrap(), result);

    let hold = control.prepare_session_method(&captured, "hold", serde_json::json!({}), timeout).unwrap();
    let cancellation = CancellationToken::new();
    let call = {
        let (control, hold, cancellation) = (control.clone(), hold.clone(), cancellation.clone());
        tokio::spawn(async move { control.call_session_method(&hold, &cancellation).await })
    };
    while !jvm.sees(&format!("method/{}", hold.operation_id())) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    cancellation.cancel();
    assert_eq!(call.await.unwrap().unwrap().phase(), SessionMethodPhase::Cancelled);
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
    let generation = first.upserts.iter().find(|entry| entry.key == "delivery/login").and_then(|entry| {
        let Some(State::Value(value)) = &entry.state else { return None };
        JvmDelivery::decode(value.as_slice()).unwrap().generation
    });
    let lost = Some(Position { epoch: 1, revision: 1 });
    let report = JvmReport {
        complete: true,
        sessions: vec![JvmSessionStatus { attached: 2, ..session(&id, JvmSessionPhase::Ready) }],
        deliveries: vec![
            delivery("login", generation, JvmDeliveryPhase::Arrived),
            delivery("lost", lost, JvmDeliveryPhase::Arrived),
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
    drop(updates);
    fixture.stop().await;
}
