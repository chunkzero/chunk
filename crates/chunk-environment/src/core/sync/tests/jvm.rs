//! A JVM's topic, registration and reports over the sync protocol.

use super::*;
use chunk_control::{Progress, RuntimeConnection};
use chunk_proto::{
    sync::v1::{JvmHealth, JvmRegistered, JvmRegistration, JvmReport, JvmSession, JvmSessionPhase, JvmSessionStatus},
    v1::{NodePhase, ProcessIdentity, ProcessRegistration},
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
    async fn jvm_call(
        &mut self,
        credential: &str,
        method: &str,
        stream: &str,
        arguments: &impl Message,
    ) -> CallResponse {
        let message = CallRequest {
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            stream: stream.into(),
            ..CallRequest::default()
        };
        self.client.call(authorized(message, credential)).await.unwrap().into_inner()
    }

    async fn register(&mut self) -> JvmRegistered {
        let response = self.jvm_call(JVM, "chunk:register", "", &registration()).await;
        match response.outcome {
            Some(Outcome::Result(result)) => JvmRegistered::decode(result.as_slice()).unwrap(),
            outcome => panic!("expected a registration, got {outcome:?}"),
        }
    }

    async fn follow_jvm(&mut self, credential: &str, host: &str) -> Streaming<Update> {
        let subscription = SubscribeRequest { topic: format!("jvm/{host}"), ..SubscribeRequest::default() };
        self.client.subscribe(authorized(subscription, credential)).await.unwrap().into_inner()
    }
}

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
    let mut fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let host = place(&fixture, &jvm).await;
    assert_eq!(fixture.register().await.host, host);

    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    let (id, wanted) = sessions(&first).pop_first().expect("a session");
    assert_eq!((wanted.session_type.as_str(), wanted.capacity, wanted.finish), ("bridge/default", 8, false));

    let health = JvmHealth { ready: true, tick_count: 100, ..JvmHealth::default() };
    let report =
        JvmReport { complete: true, sessions: vec![session(&id, JvmSessionPhase::Ready)], health: Some(health) };
    let reported = fixture.jvm_call(JVM, "chunk:report", &first.stream, &report).await;
    assert_eq!(reported.outcome, Some(Outcome::Result(Vec::new())));
    // Control's next health pass reads the pushed sample.
    let online = async {
        loop {
            let nodes = fixture.control.nodes().unwrap().nodes;
            if let Some(node) = nodes.iter().find(|node| node.host_id == host && node.phase() == NodePhase::Online) {
                return node.health.clone().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert_eq!(tokio::time::timeout(Duration::from_secs(15), online).await.unwrap().tick_count, 100);

    let ended = JvmReport { sessions: vec![session(&id, JvmSessionPhase::Ended)], ..JvmReport::default() };
    fixture.jvm_call(JVM, "chunk:report", &first.stream, &ended).await;
    while !sessions(&next(&mut updates).await).get(&id).is_some_and(|wanted| wanted.finish) {}
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_jvm_stream_and_its_reports_are_stopped() {
    let mut fixture = Fixture::with_host(Arc::new(Launches::of("host-1", false))).await;
    fixture.register().await;
    let mut older = fixture.follow_jvm(JVM, "host-1").await;
    let superseded = next(&mut older).await;
    let mut newer = fixture.follow_jvm(JVM, "host-1").await;
    let current = next(&mut newer).await;

    assert_eq!(next(&mut older).await.error.map(|error| error.code()), Some(Code::Stopped));
    let report = JvmReport { complete: true, ..JvmReport::default() };
    let stale = fixture.jvm_call(JVM, "chunk:report", &superseded.stream, &report).await;
    assert_eq!(code(&stale), Code::Stopped);
    let partial = fixture.jvm_call(JVM, "chunk:report", &current.stream, &JvmReport::default()).await;
    assert_eq!(code(&partial), Code::Invalid);
    let attached = fixture.jvm_call(JVM, "chunk:report", &current.stream, &report).await;
    assert_eq!(attached.outcome, Some(Outcome::Result(Vec::new())));
    drop((older, newer));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_jvm_that_outlived_core_re_attaches_by_registering() {
    let jvm = Launches::default();
    let mut fixture = Fixture::with_host(Arc::new(jvm.clone())).await;
    let host = place(&fixture, &jvm).await;
    fixture.register().await;
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let (id, _) = sessions(&next(&mut updates).await).pop_first().expect("a session");
    drop(updates);

    let mut fixture = fixture.restart(Arc::new(Launches::of(&host, true))).await;
    let mut unadopted = fixture.follow_jvm(JVM, &host).await;
    assert_eq!(next(&mut unadopted).await.error.map(|error| error.code()), Some(Code::Denied));
    let report = JvmReport { complete: true, ..JvmReport::default() };
    assert_eq!(code(&fixture.jvm_call(JVM, "chunk:report", "", &report).await), Code::Denied);

    assert_eq!(fixture.register().await.host, host);
    let mut updates = fixture.follow_jvm(JVM, &host).await;
    let first = next(&mut updates).await;
    assert!(sessions(&first).contains_key(&id));
    let report = JvmReport { complete: true, sessions: vec![session(&id, JvmSessionPhase::Ready)], health: None };
    let attached = fixture.jvm_call(JVM, "chunk:report", &first.stream, &report).await;
    assert_eq!(attached.outcome, Some(Outcome::Result(Vec::new())));
    drop(updates);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_hosts_own_jvm_follows_its_topic_and_calls_its_methods() {
    let mut fixture = Fixture::start().await;
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
