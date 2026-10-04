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

/// Object storage in memory, standing in for the buckets management grants.
#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);

impl Memory {
    fn keys(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }
}

impl chunk_store::ObjectStorage for Memory {
    fn put(&self, key: &str, bytes: Vec<u8>) -> std::io::Result<()> {
        self.0.lock().unwrap().insert(key.into(), bytes);
        Ok(())
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> std::io::Result<bool> {
        let mut objects = self.0.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        objects.insert(key.into(), bytes);
        Ok(true)
    }

    fn get(&self, key: &str) -> std::io::Result<Vec<u8>> {
        let objects = self.0.lock().unwrap();
        objects.get(key).cloned().ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }

    fn list(&self, prefix: &str) -> std::io::Result<Vec<chunk_store::Listed>> {
        let objects = self.0.lock().unwrap();
        let below = objects.iter().filter(|(key, _)| key.starts_with(&format!("{prefix}/")));
        let listed = below.map(|(key, bytes)| chunk_store::Listed {
            key: key.clone(),
            size: bytes.len() as u64,
            modified: std::time::SystemTime::now(),
        });
        Ok(listed.collect())
    }

    fn delete(&self, key: &str) -> std::io::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fork_s_core_forks_the_named_log_and_reports_its_newest_deployment_without_serving_it() {
    let harness = Harness::new().await;
    // The source replicated two deployments that the fork's control never ran.
    let (source, target) = (Arc::new(Memory::default()), Arc::new(Memory::default()));
    let replicated = |storage: &Arc<Memory>| chunk_store::Replication::new(storage.clone());
    let state = harness.directory.path().join("source");
    let source_core = CoreConfig {
        environment: "env_source".into(),
        control_record: state.join("control.json"),
        assets: state.join("assets"),
        state,
        replication: Some(replicated(&source)),
        ..harness.core()
    };
    let core = Core::start(source_core, || {}).await.unwrap();
    harness.abandon_in(&core, 2).await;
    core.stop(|| {}).await.unwrap();

    let grant = |prefix: &str| ObjectStore {
        endpoint: "http://127.0.0.1:9".into(),
        bucket: "logs".into(),
        prefix: prefix.into(),
        access_key_id: "key".into(),
        secret_access_key: "secret".into(),
        ..ObjectStore::default()
    };
    *harness.management.restore.lock().unwrap() = Some(Restore {
        source: Some(grant("env_source/")),
        snapshot_id: String::new(),
        source_environment_id: "env_source".into(),
    });
    harness.management.grant(grant("env_test/"));
    let mut config = harness.core();
    let stop = CancellationToken::new();
    let registration =
        Registration::attach(&harness.management_config(), &mut config, &stop).await.unwrap().expect("attached");
    let fork = config.fork.as_mut().expect("a fork source");
    assert_eq!((fork.environment.as_str(), fork.snapshot), ("env_source", None));

    // The registered configuration starts core, with the granted buckets in memory.
    fork.replication = replicated(&source);
    config.replication = Some(replicated(&target));
    let core = Core::start(config, || {}).await.unwrap();
    assert!(target.keys().iter().any(|key| key.contains("/snapshots/")), "{:?}", target.keys());
    let (gateway, lease) = (OnceLock::new(), watch::Sender::new(Lease::Waiting));
    let managed = Managed::new(
        &harness.management_config(),
        lease,
        registration,
        &harness.state(),
        &core,
        &gateway,
        Some(GatewayConfig::new("127.0.0.1:0".parse().unwrap())),
    );
    let reported = async {
        while harness.management.restored.lock().unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    tokio::select! {
        error = managed.run() => panic!("{error}"),
        () = reported => {}
    }
    drop(managed);
    assert_eq!(*harness.management.restored.lock().unwrap(), ["dep_abandoned_1"]);
    assert!(gateway.get().is_none());
    assert_eq!(core.control().unwrap().current_release().unwrap(), None);
    core.stop(|| {}).await.unwrap();
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
