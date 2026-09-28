//! Core's shutdown: it refuses new gateway operations, awaits those it admitted, stops hosts, and releases the
//! environment's authority before `run` returns.

use super::{
    claims::{login, result},
    jvm::Launches,
    jvm_effects::{SyncJvm, arrive, phase},
    *,
};
use chunk_proto::sync::v1::{ClaimPhase, ClaimRefusal, ClaimResult, JvmDeliveryPhase, WithdrawResult, claim_result};

/// Gateway `proxy`'s calls on its current stream, each through a client of its own.
#[derive(Clone)]
struct Gateway {
    client: CoreClient<Channel>,
    credential: String,
    stream: String,
}

impl Gateway {
    /// Follows the gateway's topic, returning its stream and a handle that calls on it.
    async fn follow(fixture: &mut Fixture) -> (Streaming<Update>, Self) {
        let credential = fixture.gateway.clone();
        let (updates, first) = fixture.follow(&credential, "proxy").await;
        (updates, Self { client: fixture.client.clone(), credential, stream: first.stream })
    }

    async fn call(&self, operation: &str, method: &str, arguments: &impl Message) -> CallResponse {
        let message = CallRequest {
            operation_id: operation.into(),
            method: method.into(),
            arguments: arguments.encode_to_vec(),
            stream: self.stream.clone(),
            ..CallRequest::default()
        };
        self.client.clone().call(authorized(message, &self.credential)).await.unwrap().into_inner()
    }
}

/// Whether core refused `response`'s call because control is shutting down.
fn draining(response: &CallResponse) -> bool {
    matches!(&response.outcome, Some(Outcome::Error(error)) if error.message.ends_with("control draining"))
}

/// Logs the fake player in again under a new operation until core refuses it as draining. Until then control refuses
/// it at once, since the player already holds a claim.
async fn refused(gateway: &Gateway) {
    loop {
        let response = gateway.call("reconnect", "chunk:claim", &login("reconnect")).await;
        if draining(&response) {
            return;
        }
        let refusal = result::<ClaimResult>(&response).outcome;
        assert_eq!(refusal, Some(claim_result::Outcome::Refusal(ClaimRefusal::AlreadyConnected.into())));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_refuses_new_claims_while_it_awaits_admitted_ones() {
    let launches = Launches::default();
    let mut fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    fixture.control.activate_release(runtime::release()).unwrap();
    let (updates, gateway) = Gateway::follow(&mut fixture).await;
    // Until its JVM registers, an admitted claim waits for it. Control launches the host once the claim reserved it.
    let admitted = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.call("login", "chunk:claim", &login("connection")).await }
    });
    let host = loop {
        if let Some(host) = launches.host() {
            break host;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };

    fixture.stop.cancel();
    refused(&gateway).await;
    assert!(!fixture.task.is_finished());
    // Once the JVM registers, the admitted claim completes and shutdown stops its host.
    let _jvm = SyncJvm::connect(fixture.client.clone(), &host).await;
    let claimed = result::<ClaimResult>(&admitted.await.unwrap());
    assert!(matches!(claimed.outcome, Some(claim_result::Outcome::Assignment(_))));
    drop(updates);
    fixture.stop().await;
    assert!(launches.0.lock().unwrap().released);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_refuses_withdrawal_retries_while_a_withdrawal_stalls() {
    let launches = Launches::default();
    let mut fixture = Fixture::with_host(Arc::new(launches.clone())).await;
    let (jvm, _) = arrive(&fixture, &launches).await;
    let (updates, gateway) = Gateway::follow(&mut fixture).await;
    // The JVM stalls the withdrawal, which core admitted.
    jvm.stall_withdrawals();
    let withdrawal = tokio::spawn({
        let gateway = gateway.clone();
        async move { gateway.call("login", "chunk:withdraw", &()).await }
    });
    phase(&fixture, Some(ClaimPhase::Withdrawing)).await;

    fixture.stop.cancel();
    refused(&gateway).await;
    // The gateway keeps retrying, alternating cancellation and departure, and core refuses every retry.
    for method in ["chunk:withdraw", "chunk:depart"].repeat(2) {
        assert!(draining(&gateway.call("login", method, &()).await), "core admitted a retry");
    }
    assert!(!fixture.task.is_finished());
    // Once the JVM closes the delivery, the admitted withdrawal completes and shutdown stops the host.
    jvm.player("login", JvmDeliveryPhase::Closed).await;
    assert!(!result::<WithdrawResult>(&withdrawal.await.unwrap()).unknown);
    drop(updates);
    fixture.stop().await;
    assert!(launches.0.lock().unwrap().released);
}

/// Serves control again on `fixture`'s backend, which fails while the stopped control still holds the environment's
/// authority.
async fn reopen(fixture: &Fixture) -> io::Result<()> {
    let (ready, started) = oneshot::channel();
    let stop = CancellationToken::new();
    let config = chunk_control::server::Config {
        state: fixture.directory.path().join("control"),
        system: fixture.backend.system(),
        connection: fixture.directory.path().join("control.json"),
        listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        network: None,
        control: chunk_control::Config { environment: "test".into() },
        host: Arc::new(Host),
        fresh: false,
        services: None,
    };
    let task = tokio::spawn(chunk_control::server::run(config, ready, stop.clone()));
    if started.await.is_ok() {
        stop.cancel();
    }
    task.await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_releases_authority_before_returning_with_an_open_stream() {
    const ATTEMPTS: usize = 20;
    for attempt in 1..=ATTEMPTS {
        let mut fixture = Fixture::start().await;
        // Keep the gateway's stream open through shutdown and the immediate reopen.
        let (updates, _) = Gateway::follow(&mut fixture).await;
        fixture.stop.cancel();
        (&mut fixture.task).await.unwrap().unwrap();
        // No yield or retry: returning from run must release the environment's authority.
        let reopened = reopen(&fixture).await;
        reopened.unwrap_or_else(|error| panic!("attempt {attempt}: authority remained locked: {error}"));
        drop(updates);
        let Fixture { backend, .. } = fixture;
        tokio::task::spawn_blocking(move || drop(backend)).await.unwrap();
    }
}
