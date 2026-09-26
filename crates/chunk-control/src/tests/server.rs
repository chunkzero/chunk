use super::*;
use chunk_proto::v1::{WatchRequest, local_control_client::LocalControlClient};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn run_releases_authority_before_returning_with_an_open_watch() {
    const ATTEMPTS: usize = 20;
    let mut failures = Vec::new();
    for attempt in 1..=ATTEMPTS {
        let failure = tokio::time::timeout(Duration::from_secs(10), async {
            let fixture = Fixture::new().await;
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
