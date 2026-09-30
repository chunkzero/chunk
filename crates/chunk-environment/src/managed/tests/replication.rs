use super::*;
use crate::{
    Core,
    managed::{Lease, Managed, Registration, log_store::LogStore},
};
use object_store::CredentialProvider;
use std::sync::OnceLock;

impl Management {
    /// Grants `store` from the next desired state on.
    fn grant(&self, store: ObjectStore) {
        *self.log_store.lock().unwrap() = Some(store);
        self.publish(&mut self.records.lock().unwrap());
    }

    /// Serves the latest deployment from `release` instead.
    fn rerelease(&self, release: ReleaseArtifact) {
        let mut records = self.records.lock().unwrap();
        records.deployments.last_mut().unwrap().1 = release;
        self.publish(&mut records);
    }
}

/// A fresh prefix of the bucket the `CHUNK_REPLICATION_*` variables name, as management grants it.
fn granted() -> ObjectStore {
    let variable = |name| std::env::var(name).unwrap_or_default();
    let unique = uuid::Uuid::new_v4();
    ObjectStore {
        endpoint: variable("CHUNK_REPLICATION_ENDPOINT"),
        region: variable("CHUNK_REPLICATION_REGION"),
        bucket: variable("CHUNK_REPLICATION_BUCKET"),
        prefix: format!("{}/managed-{unique}/", variable("CHUNK_REPLICATION_PREFIX")),
        access_key_id: variable("CHUNK_REPLICATION_ACCESS_KEY_ID"),
        secret_access_key: variable("CHUNK_REPLICATION_SECRET_ACCESS_KEY"),
        ..ObjectStore::default()
    }
}

/// Run against a disposable bucket with `CHUNK_REPLICATION_*` set, for example `MinIO`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs an S3-compatible server"]
async fn core_replicates_where_management_grants_and_a_fresh_core_restores_from_there() {
    let mut harness = Harness::new().await;
    let store = granted();
    harness.management.grant(store.clone());
    harness.deploy("dep_a", harness.valid());
    let (stop, running) = harness.start();
    harness.expect(2, "dep_a", DeploymentState::InProgress).await;
    harness.expect(2, "dep_a", DeploymentState::Active).await;

    // Renewed credentials take effect without a restart: wrong ones leave dep_b's commits unflushed, so core fails.
    harness.management.grant(ObjectStore { secret_access_key: "wrong".into(), ..store.clone() });
    harness.expect(3, "dep_a", DeploymentState::Active).await;
    harness.deploy("dep_b", harness.valid());
    harness.expect(4, "dep_b", DeploymentState::InProgress).await;
    harness.expect(4, "dep_b", DeploymentState::Active).await;
    stop.cancel();
    let error = tokio::time::timeout(Duration::from_secs(60), running).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("replication"), "{error}");

    // Restarted on its volume with working credentials, core uploads what it kept and stops cleanly.
    harness.management.grant(store);
    let (stop, running) = harness.start();
    harness.expect(5, "dep_b", DeploymentState::InProgress).await;
    harness.expect(5, "dep_b", DeploymentState::Active).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(60), running).await.unwrap().unwrap().unwrap();

    // A core without its volume restores dep_b from object storage under a newer epoch, though its release can't
    // download.
    fs::remove_dir_all(harness.state()).unwrap();
    harness.management.rerelease(harness.stalled());
    let (stop, running) = harness.start();
    harness.expect(6, "dep_b", DeploymentState::InProgress).await;
    harness.management.stalled.notified().await;
    assert!(harness.serves("dep_b").await);
    assert_eq!(*harness.management.epochs.lock().unwrap(), [1, 1, 2]);
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(60), running).await.unwrap().unwrap().unwrap();
}

/// Waits until `log_store` signs with `token`.
async fn renewed(log_store: &LogStore, token: &str) {
    let credentials = log_store.credentials().unwrap();
    let renewal = async {
        while credentials.get_credential().await.unwrap().token.as_deref() != Some(token) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(Duration::from_secs(10), renewal).await.expect("credentials renewed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporary_log_store_credentials_renew_before_the_core_attach_and_after_it_ends() {
    let harness = Harness::new().await;
    let temporary = |token: &str| ObjectStore {
        endpoint: "http://127.0.0.1:9".into(),
        bucket: "logs".into(),
        prefix: "env_test/".into(),
        access_key_id: "key".into(),
        secret_access_key: "secret".into(),
        session_token: token.into(),
        ..ObjectStore::default()
    };
    harness.management.grant(temporary("opening"));
    let mut config = harness.core();
    let stop = CancellationToken::new();
    let registration =
        Registration::attach(&harness.management_config(), &mut config, &stop).await.unwrap().expect("attached");
    assert!(config.replication.is_some());
    let log_store = registration.log_store.clone();
    renewed(&log_store, "opening").await;

    // While core restores its log and starts, the attach that took no lease renews.
    harness.management.grant(temporary("restoring"));
    renewed(&log_store, "restoring").await;

    let core = Core::start(harness.core(), || {}).await.unwrap();
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let managed =
        Managed::new(&harness.management_config(), lease, registration, &harness.state(), &core, &gateway, None);
    let renewer = managed.renewer();
    let serving = async {
        while managed.renewal.lock().unwrap().is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        harness.management.grant(temporary("serving"));
        renewed(&log_store, "serving").await;
    };
    tokio::select! {
        error = managed.run() => panic!("{error}"),
        () = serving => {}
    }
    drop(managed);

    // Once the core attach ended, until the final flush.
    let renewal = renewer.start(None).expect("a log store was granted");
    harness.management.grant(temporary("flushing"));
    renewed(&log_store, "flushing").await;
    renewal.stop().await;
    core.stop(|| {}).await.unwrap();
}
