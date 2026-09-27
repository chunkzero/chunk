use super::*;
use chunk_proto::v1::{WatchRequest, local_control_client::LocalControlClient};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn run_releases_authority_before_returning_with_an_open_watch() {
    const ATTEMPTS: usize = 20;
    let mut failures = Vec::new();
    for attempt in 1..=ATTEMPTS {
        let failure = tokio::time::timeout(Duration::from_secs(10), async {
            let fixture = Fixture::new();
            let environment = environment(&fixture.release);
            let store = chunk_store::SqliteStore::open(
                fixture.directory.path().join("control.sqlite"),
                &environment.environment,
            )
            .unwrap();
            let backend = chunk_backend::Backend::new(environment.environment.clone(), Box::new(store)).unwrap();
            let system = backend.system();
            let stop = CancellationToken::new();
            let (ready, connection) = oneshot::channel();
            let config = crate::server::Config {
                state: fixture.directory.path().join("state"),
                system: system.clone(),
                connection: fixture.directory.path().join("control.json"),
                listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
                control: environment.clone(),
                host: fixture.host.clone(),
                fresh: false,
                services: None,
            };
            let server = async {
                crate::server::run(config, ready, stop.clone()).await.unwrap();
                // No yield or retry: returning from run must release the environment's lock.
                Control::open(system, environment.clone(), fixture.host.clone(), false)
            };
            let watch = async {
                let connection = connection.await.unwrap().connection;
                let mut client = LocalControlClient::connect(connection.endpoint).await.unwrap();
                let mut request = Request::new(WatchRequest { proxy_id: "proxy-1".into() });
                request.metadata_mut().insert("authorization", format!("Bearer {}", connection.token).parse().unwrap());
                let mut updates = client.watch(request).await.unwrap().into_inner();
                let snapshot = updates.message().await.unwrap().expect("watch must send its snapshot");
                assert!(snapshot.snapshot);
                assert!(snapshot.claims.is_empty());
                assert!(fixture.host.ids.lock().unwrap().is_empty());
                stop.cancel();
                (client, updates)
            };
            // Keep the client and stream alive through shutdown and the immediate reopen.
            let (reopened, watch) = tokio::join!(server, watch);
            let failure = reopened.err().map(|error| format!("attempt {attempt}: {error}"));
            drop(watch);
            drop(backend);
            fixture.close().await;
            failure
        })
        .await
        .unwrap_or_else(|_| panic!("attempt {attempt} timed out"));
        if let Some(failure) = failure {
            failures.push(failure);
        }
    }
    assert!(
        failures.is_empty(),
        "authority remained locked after run returned in {}/{ATTEMPTS} attempts:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Serves control for `fixture` until the returned token is cancelled.
async fn serve(
    fixture: &Fixture,
) -> (JoinHandle<std::io::Result<()>>, crate::server::Ready, CancellationToken, chunk_backend::Backend) {
    let environment = environment(&fixture.release);
    let store =
        chunk_store::SqliteStore::open(fixture.directory.path().join("control.sqlite"), &environment.environment)
            .unwrap();
    let backend = chunk_backend::Backend::new(environment.environment.clone(), Box::new(store)).unwrap();
    let stop = CancellationToken::new();
    let (ready, connection) = oneshot::channel();
    let config = crate::server::Config {
        state: fixture.directory.path().join("state"),
        system: backend.system(),
        connection: fixture.directory.path().join("control.json"),
        listener: tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap(),
        control: environment,
        host: fixture.host.clone(),
        fresh: false,
        services: None,
    };
    let server = tokio::spawn(crate::server::run(config, ready, stop.clone()));
    (server, connection.await.unwrap(), stop, backend)
}

fn authorized<T>(token: &str, message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.metadata_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

#[tokio::test]
async fn shutdown_refuses_new_claims_while_it_awaits_admitted_ones() {
    let fixture = Fixture::new();
    let (server, crate::server::Ready { connection, control }, stop, backend) = serve(&fixture).await;
    control.activate_release(fixture.release.clone()).unwrap();
    // While its JVM starts, an admitted claim waits for it.
    fixture.host.starting.store(true, Ordering::Release);
    let jvm = CancellationToken::new();
    let follower = tokio::spawn(follow(control.clone(), fixture.host.clone(), jvm.clone()));
    let mut client = LocalControlClient::connect(connection.endpoint).await.unwrap();
    let admitted = tokio::spawn({
        let (mut client, request) =
            (client.clone(), authorized(&connection.token, request("claim-1", &uuid::Uuid::new_v4().to_string())));
        async move { client.claim(request).await }
    });
    eventually(|| !fixture.host.ids.lock().unwrap().is_empty()).await;

    stop.cancel();
    eventually(|| control.draining.load(Ordering::Acquire)).await;
    let claim = request("claim-2", &uuid::Uuid::new_v4().to_string());
    let refused = client.claim(authorized(&connection.token, claim)).await.unwrap_err();
    assert_eq!((refused.code(), refused.message()), (tonic::Code::FailedPrecondition, "control draining"));
    assert!(!server.is_finished());
    // Once the JVM has started, the admitted claim completes and shutdown stops its host.
    fixture.host.starting.store(false, Ordering::Release);
    admitted.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap().unwrap();
    assert_eq!(*fixture.host.terminated.lock().unwrap(), *fixture.host.ids.lock().unwrap());

    jvm.cancel();
    follower.await.unwrap();
    drop((client, control, backend));
    fixture.close().await;
}

#[tokio::test]
async fn shutdown_refuses_withdrawal_retries_while_a_withdrawal_stalls() {
    let fixture = Fixture::new();
    let (server, crate::server::Ready { connection, control }, stop, backend) = serve(&fixture).await;
    control.activate_release(fixture.release.clone()).unwrap();
    let jvm = CancellationToken::new();
    let follower = tokio::spawn(follow(control.clone(), fixture.host.clone(), jvm.clone()));
    let mut client = LocalControlClient::connect(connection.endpoint).await.unwrap();
    let claim = request("claim-1", &uuid::Uuid::new_v4().to_string());
    client.claim(authorized(&connection.token, claim.clone())).await.unwrap();

    // The JVM stalls the withdrawal, and a proxy keeps retrying it, alternating cancellation and departure, each retry
    // abandoned after a moment and left running on control.
    fixture.runtime.stalled_withdrawal.store(true, Ordering::Release);
    let refusals = Arc::new(AtomicUsize::new(0));
    let admitted_after_refusal = Arc::new(AtomicBool::new(false));
    let done = CancellationToken::new();
    let retries = tokio::spawn({
        let (refusals, admitted_after_refusal, done) = (refusals.clone(), admitted_after_refusal.clone(), done.clone());
        async move {
            for attempt in 0usize.. {
                if done.is_cancelled() {
                    break;
                }
                let request = authorized(&connection.token, claim.clone());
                let retry = async {
                    if attempt % 2 == 0 {
                        client.cancel(request).await.map(drop)
                    } else {
                        client.reconcile_departure(request).await.map(drop)
                    }
                };
                match tokio::time::timeout(Duration::from_millis(20), retry).await {
                    Ok(Err(status))
                        if (status.code(), status.message())
                            == (tonic::Code::FailedPrecondition, "control draining") =>
                    {
                        refusals.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(Ok(())) if refusals.load(Ordering::Acquire) > 0 => {
                        admitted_after_refusal.store(true, Ordering::Release);
                    }
                    _ => {}
                }
            }
        }
    });
    eventually(|| {
        control.state().unwrap().claims.get("claim-1").is_some_and(|claim| claim.phase == Phase::Withdrawing)
    })
    .await;

    stop.cancel();
    eventually(|| refusals.load(Ordering::Acquire) > 0).await;
    assert!(!server.is_finished());
    // Once the JVM answers, the admitted withdrawals finish and shutdown stops the host while retries still arrive.
    fixture.runtime.stalled_withdrawal.store(false, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(10), server).await.unwrap().unwrap().unwrap();
    assert_eq!(*fixture.host.terminated.lock().unwrap(), *fixture.host.ids.lock().unwrap());
    done.cancel();
    retries.await.unwrap();
    assert!(!admitted_after_refusal.load(Ordering::Acquire));

    jvm.cancel();
    follower.await.unwrap();
    drop((control, backend));
    fixture.close().await;
}
