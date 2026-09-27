//! A JVM's topic, registration and reports over the sync protocol.

use super::*;
use chunk_control::{Progress, RuntimeConnection};
use chunk_proto::{
    sync::v1::{JvmHealth, JvmRegistered, JvmRegistration, JvmReport, JvmSession, JvmSessionPhase, JvmSessionStatus},
    v1::{NodePhase, NodeStatus, ProcessIdentity, ProcessRegistration},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};

/// A host running one JVM, which registers over sync with credential `JVM`: the first host control ensures, or a
/// given one. A survivor was launched before core restarted and awaits re-attachment.
#[derive(Clone, Default)]
struct Launches(Arc<Mutex<Launch>>);

#[derive(Default)]
struct Launch {
    host: Option<String>,
    registration: Option<ProcessRegistration>,
    survivor: bool,
    released: bool,
}

impl Launches {
    fn of(host: &str, survivor: bool) -> Self {
        let launch = Launch { host: Some(host.into()), survivor, ..Launch::default() };
        Self(Arc::new(Mutex::new(launch)))
    }

    fn host(&self) -> Option<String> {
        self.0.lock().unwrap().host.clone()
    }

    fn awaiting(launch: &Launch) -> bool {
        launch.survivor && launch.registration.is_none()
    }
}

#[tonic::async_trait]
impl chunk_control::Host for Launches {
    async fn ensure(&self, id: &str, _: &chunk_control::Release, _: &str, _: &str) -> chunk_control::Result<Progress> {
        let host = self.0.lock().unwrap().host.get_or_insert_with(|| id.into()).clone();
        if self.stopped(id) {
            return Ok(Progress::Failed("released".into()));
        }
        Ok(self.connection(&host).filter(|_| host == id).map_or(Progress::Pending, |c| Progress::Ready(Box::new(c))))
    }
    async fn release(&self, _: &str) -> chunk_control::Result<bool> {
        self.0.lock().unwrap().released = true;
        Ok(true)
    }
    fn stopped(&self, _: &str) -> bool {
        self.0.lock().unwrap().released
    }
    fn unresolved(&self, id: &str) -> bool {
        self.unowned().unwrap().contains(id)
    }
    fn unowned(&self) -> chunk_control::Result<BTreeSet<String>> {
        let launch = self.0.lock().unwrap();
        Ok(launch.host.iter().filter(|_| Self::awaiting(&launch)).cloned().collect())
    }
    fn register(&self, token: &str, registration: ProcessRegistration) -> chunk_control::Result<ProcessIdentity> {
        let mut launch = self.0.lock().unwrap();
        let identity = registration.identity.clone().unwrap_or_default();
        if Self::awaiting(&launch)
            || launch.host != Some(identity.runtime_id.clone())
            || token != format!("Bearer {JVM}")
        {
            return Err(chunk_control::Error::Invalid("unknown process"));
        }
        if launch.registration.as_ref().is_some_and(|frozen| *frozen != registration) {
            return Err(chunk_control::Error::Invalid("registration changed"));
        }
        launch.registration = Some(registration);
        Ok(identity)
    }
    fn adopt(&self, token: &str, registration: ProcessRegistration) -> chunk_control::Result<()> {
        let mut launch = self.0.lock().unwrap();
        if !Self::awaiting(&launch) || token != JVM {
            return Err(chunk_control::Error::Invalid("process is not awaiting re-attachment"));
        }
        launch.registration = Some(registration);
        Ok(())
    }
    fn connection(&self, id: &str) -> Option<RuntimeConnection> {
        let launch = self.0.lock().unwrap();
        let registration = launch.registration.clone().filter(|_| launch.host.as_deref() == Some(id))?;
        Some(RuntimeConnection {
            endpoint: registration.control_endpoint,
            token: JVM.into(),
            identity: registration.identity?,
            player_endpoint: registration.player_endpoint,
        })
    }
    fn authenticate(&self, credential: &str) -> Option<String> {
        let launch = self.0.lock().unwrap();
        launch.host.clone().filter(|_| credential == JVM && !Self::awaiting(&launch) && !launch.released)
    }
    fn unadopted(&self, credential: &str) -> Option<String> {
        let launch = self.0.lock().unwrap();
        launch.host.clone().filter(|_| credential == JVM && Self::awaiting(&launch))
    }
}

fn registration() -> JvmRegistration {
    JvmRegistration {
        process_id: "jvm".into(),
        generation: 1,
        app: "bridge".into(),
        profile: "small".into(),
        artifact_digest: "digest".into(),
        deployment: "test".into(),
        player_endpoint: "127.0.0.1:1".into(),
        protocol: 776,
    }
}

fn session(id: &str, phase: JvmSessionPhase) -> JvmSessionStatus {
    JvmSessionStatus {
        id: id.into(),
        session_type: "bridge/default".into(),
        capacity: 8,
        phase: phase.into(),
        ..JvmSessionStatus::default()
    }
}

impl Fixture {
    async fn jvm_call(&self, credential: &str, method: &str, stream: &str, arguments: &impl Message) -> CallResponse {
        let message = CallRequest {
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        self.client.clone().call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn register(&self) -> JvmRegistered {
        let response = self.jvm_call(JVM, "chunk:register", "", &registration()).await;
        match response.outcome {
            Some(Outcome::Result(result)) => JvmRegistered::decode(result.as_slice()).unwrap(),
            outcome => panic!("expected a registration, got {outcome:?}"),
        }
    }

    async fn report(&self, stream: &str, report: &JvmReport) -> CallResponse {
        self.jvm_call(JVM, "chunk:report", stream, report).await
    }

    async fn follow_jvm(&self, credential: &str, host: &str) -> Streaming<Update> {
        let subscription = SubscribeRequest { topic: format!("jvm/{host}"), ..SubscribeRequest::default() };
        self.client.clone().subscribe(authorized(subscription, credential)).await.unwrap().into_inner()
    }

    /// Waits for control's health pass to leave `host`'s node `matching`.
    async fn node(&self, host: &str, matching: impl Fn(&NodeStatus) -> bool) {
        let found = async {
            loop {
                let nodes = self.control.nodes().unwrap().nodes;
                if nodes.iter().any(|node| node.host_id == host && matching(node)) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(30), found).await.expect("the node's phase");
    }
}

const ACCEPTED: Option<Outcome> = Some(Outcome::Result(Vec::new()));

/// The sessions a snapshot asks for, by ID.
fn sessions(update: &Update) -> BTreeMap<String, JvmSession> {
    assert!(update.snapshot && update.error.is_none());
    let sessions = update.upserts.iter().filter_map(|entry| match &entry.state {
        Some(State::Value(value)) => {
            Some((entry.key.strip_prefix("session/")?.to_owned(), JvmSession::decode(value.as_slice()).unwrap()))
        }
        _ => None,
    });
    sessions.collect()
}

fn stopped(update: &Update) -> bool {
    update.error.as_ref().is_some_and(|error| error.code() == Code::Stopped)
}

fn complete(id: &str, phase: JvmSessionPhase, health: Option<JvmHealth>) -> JvmReport {
    JvmReport { complete: true, sessions: vec![session(id, phase)], health }
}

fn health(tick_count: u64, draining: bool) -> JvmHealth {
    JvmHealth { ready: true, draining, tick_count, ..JvmHealth::default() }
}

/// Reserves a session for the fake player on the fake JVM's host, returning the host once it launched. The claim
/// itself fails, since the JVM serves no legacy gameplay endpoint.
async fn place(fixture: &Fixture, jvm: &Launches) -> String {
    fixture.control.activate_release(runtime::release()).unwrap();
    let control = fixture.control.clone();
    drop(tokio::spawn(async move { control.claim(runtime::login()).await }));
    loop {
        if let Some(host) = jvm.host() {
            return host;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_registers_follows_its_sessions_and_reports_them() {
    let jvm = Launches::default();
    let fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let host = place(&fixture, &jvm).await;
    assert_eq!(fixture.register().await.host, host);
    let changed = JvmRegistration { protocol: 777, ..registration() };
    assert_eq!(code(&fixture.jvm_call(JVM, "chunk:register", "", &changed).await), Code::Invalid);
    let unknown = JvmRegistration { deployment: "missing".into(), ..registration() };
    assert_eq!(code(&fixture.jvm_call(JVM, "chunk:register", "", &unknown).await), Code::Contract);

    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    let mut wanted = sessions(&first);
    let (id, asked) = wanted.first_key_value().map(|(id, asked)| (id.clone(), asked.clone())).expect("a session");
    assert_eq!((asked.session_type.as_str(), asked.capacity, asked.finish), ("bridge/default", 8, false));

    let ready = complete(&id, JvmSessionPhase::Ready, Some(health(100, false)));
    assert_eq!(fixture.report(&first.stream, &ready).await.outcome, ACCEPTED);
    let ended = JvmReport { sessions: vec![session(&id, JvmSessionPhase::Ended)], ..JvmReport::default() };
    assert_eq!(fixture.report(&first.stream, &ended).await.outcome, ACCEPTED);
    // Commits that leave the JVM's entries as they were send it nothing.
    loop {
        let update = sessions(&next(&mut updates).await);
        assert_ne!(update, wanted);
        if update.get(&id).is_some_and(|wanted| wanted.finish) {
            break;
        }
        wanted = update;
    }
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_jvm_streams_resolve_to_one_current_stream() {
    let fixture = Fixture::with_host(Arc::new(Launches::of("host-1", false))).await;
    fixture.register().await;
    let attach = JvmReport { complete: true, ..JvmReport::default() };
    let (mut one, mut two) = tokio::join!(fixture.follow_jvm(JVM, "host-1"), fixture.follow_jvm(JVM, "host-1"));
    // Both streams have opened once each has sent something; a superseded one may send only its end.
    let firsts = [next(&mut one).await, next(&mut two).await];
    let mut current = Vec::new();
    for (updates, mut last) in [&mut one, &mut two].into_iter().zip(firsts) {
        if last.error.is_none() {
            let reported = fixture.report(&last.stream, &attach).await;
            if reported.outcome == ACCEPTED {
                current.push(updates);
                continue;
            }
            assert_eq!(code(&reported), Code::Stopped);
            last = next(updates).await;
        }
        assert!(stopped(&last));
    }
    assert_eq!(current.len(), 1);

    let mut newer = fixture.follow_jvm(JVM, "host-1").await;
    let stream = next(&mut newer).await.stream;
    assert!(stopped(&next(current.pop().unwrap()).await));
    assert_eq!(code(&fixture.report(&stream, &JvmReport::default()).await), Code::Invalid);
    // Racing complete reports attach the stream once, so later reports still find its link.
    let (a, b) = tokio::join!(fixture.report(&stream, &attach), fixture.report(&stream, &attach));
    assert_eq!((a.outcome, b.outcome), (ACCEPTED, ACCEPTED));
    assert_eq!(fixture.report(&stream, &JvmReport::default()).await.outcome, ACCEPTED);
    drop((one, two, newer));
    fixture.stop().await;
}

/// Runs on one thread so the health pass's clock can be paused.
#[tokio::test]
async fn a_superseded_streams_reports_change_nothing_and_pushed_health_expires() {
    let jvm = Launches::default();
    let fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let host = place(&fixture, &jvm).await;
    fixture.register().await;
    let mut older = fixture.follow_jvm(JVM, &host).await;
    let superseded = next(&mut older).await;
    let (id, _) = sessions(&superseded).pop_first().expect("a session");
    let ready = complete(&id, JvmSessionPhase::Ready, Some(health(100, false)));
    assert_eq!(fixture.report(&superseded.stream, &ready).await.outcome, ACCEPTED);

    let mut newer = fixture.follow_jvm(JVM, &host).await;
    let current = next(&mut newer).await;
    assert!(stopped(&next(&mut older).await));
    let ready = complete(&id, JvmSessionPhase::Ready, Some(health(200, false)));
    assert_eq!(fixture.report(&current.stream, &ready).await.outcome, ACCEPTED);
    let sampled = tokio::time::Instant::now();
    // The superseded stream's report neither ends the session, nor drains the host, nor detaches the current link.
    let stale = complete(&id, JvmSessionPhase::Ended, Some(health(300, true)));
    assert_eq!(code(&fixture.report(&superseded.stream, &stale).await), Code::Stopped);
    assert_eq!(fixture.report(&current.stream, &JvmReport::default()).await.outcome, ACCEPTED);
    let mut latest = fixture.follow_jvm(JVM, &host).await;
    let replacement = next(&mut latest).await;
    let attach = complete(&id, JvmSessionPhase::Ready, None);
    assert_eq!(fixture.report(&replacement.stream, &attach).await.outcome, ACCEPTED);

    tokio::time::pause();
    let sample = |node: &NodeStatus| node.health.as_ref().is_some_and(|health| health.tick_count == 200);
    fixture.node(&host, |node| node.phase() == NodePhase::Online && sample(node)).await;
    // Reports without health don't renew the sample: without one for 10 seconds, the JVM counts as unhealthy.
    let unhealthy = fixture.node(&host, |node| node.phase() == NodePhase::Unhealthy);
    let reporting = async {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            assert_eq!(fixture.report(&replacement.stream, &JvmReport::default()).await.outcome, ACCEPTED);
        }
    };
    tokio::select! {
        () = unhealthy => {}
        () = reporting => {}
    }
    assert!(sampled.elapsed() >= Duration::from_secs(10));
    tokio::time::resume();
    drop((older, newer, latest));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_asks_a_sync_jvm_to_stop_before_revoking_it() {
    let jvm = Launches::default();
    let fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let host = place(&fixture, &jvm).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    let (id, _) = sessions(&first).pop_first().expect("a session");
    let ready = complete(&id, JvmSessionPhase::Ready, None);
    assert_eq!(fixture.report(&first.stream, &ready).await.outcome, ACCEPTED);

    let control = fixture.control.clone();
    let shutdown = tokio::spawn(async move { control.shutdown().await });
    while !next(&mut updates).await.upserts.iter().any(|entry| entry.key == "stop") {}
    // The stopping JVM keeps its credential until it closes its stream.
    let ending = JvmReport { sessions: vec![session(&id, JvmSessionPhase::Ending)], ..JvmReport::default() };
    assert_eq!(fixture.report(&first.stream, &ending).await.outcome, ACCEPTED);
    drop(updates);
    tokio::time::timeout(Duration::from_secs(10), shutdown).await.unwrap().unwrap().unwrap();
    // The stopped JVM's credential no longer opens its topic.
    let subscription = SubscribeRequest { topic: format!("jvm/{host}"), ..SubscribeRequest::default() };
    let again = fixture.client.clone().subscribe(authorized(subscription, JVM)).await;
    assert_eq!(again.err().map(|status| status.code()), Some(tonic::Code::Unauthenticated));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_that_outlived_core_re_attaches_by_registering() {
    let jvm = Launches::default();
    let fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let host = place(&fixture, &jvm).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let (id, _) = sessions(&next(&mut updates).await).pop_first().expect("a session");
    drop(updates);

    let fixture = fixture.restart(Arc::new(Launches::of(&host, true))).await;
    let mut unadopted = fixture.follow_jvm(JVM, &host).await;
    assert_eq!(next(&mut unadopted).await.error.map(|error| error.code()), Some(Code::Denied));
    let report = JvmReport { complete: true, ..JvmReport::default() };
    assert_eq!(code(&fixture.jvm_call(JVM, "chunk:report", "", &report).await), Code::Denied);

    assert_eq!(fixture.register().await.host, host);
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    assert!(sessions(&first).contains_key(&id));
    let report = complete(&id, JvmSessionPhase::Ready, None);
    assert_eq!(fixture.report(&first.stream, &report).await.outcome, ACCEPTED);
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_hosts_own_jvm_follows_its_topic_and_calls_its_methods() {
    let fixture = Fixture::start().await;
    let (cli, gateway) = (fixture.cli.clone(), fixture.gateway.clone());
    for (credential, host) in [(cli.as_str(), "host-1"), (gateway.as_str(), "host-1"), (JVM, "host-2")] {
        let mut updates = fixture.follow_jvm(credential, host).await;
        assert_eq!(next(&mut updates).await.error.map(|error| error.code()), Some(Code::Denied));
    }
    for credential in [cli.as_str(), gateway.as_str()] {
        assert_eq!(code(&fixture.jvm_call(credential, "chunk:register", "", &registration()).await), Code::Denied);
        assert_eq!(code(&fixture.jvm_call(credential, "chunk:report", "", &JvmReport::default()).await), Code::Denied);
    }
    fixture.stop().await;
}
