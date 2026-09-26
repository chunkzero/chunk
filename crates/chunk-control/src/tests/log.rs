use super::*;
use crate::{Generation, Table};
use chunk_store::{Listed, ObjectStorage, Replication, SqliteStore, Storage};

#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);

impl ObjectStorage for Memory {
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
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| std::io::ErrorKind::NotFound.into())
    }
    fn list(&self, prefix: &str) -> std::io::Result<Vec<Listed>> {
        let objects = self.0.lock().unwrap();
        let prefix = format!("{}/", prefix.trim_end_matches('/'));
        let modified = std::time::SystemTime::now();
        Ok(objects
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, bytes)| Listed { key: key.clone(), size: bytes.len() as u64, modified })
            .collect())
    }
    fn delete(&self, key: &str) -> std::io::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

const REVISION_MASK: u64 = (1 << 40) - 1;

#[tokio::test]
async fn generations_from_a_lost_tail_stay_fenced_after_a_restore_reuses_their_revisions() {
    let fixture = Fixture::new().await;
    let path = fixture.directory.path().join("control.sqlite");
    let storage = Arc::new(Memory::default());
    let control = fixture.control();
    let kept = request("kept", &uuid::Uuid::new_v4().to_string());
    let kept_assignment = control.claim(kept.clone()).await.unwrap();
    drop(control);
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage.clone())).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // These commits never reach object storage before the host is lost.
    let control = fixture.control();
    let lost = request("lost", &uuid::Uuid::new_v4().to_string());
    let outdated = control.claim(lost.clone()).await.unwrap().claim.unwrap();
    drop(control);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage)).unwrap();
    assert_eq!(store.epoch().0, 2);
    drop((store, replicator));

    let control = fixture.control();
    let state = control.state().unwrap();
    assert_eq!(state.epoch, 2);
    assert!(state.claims.contains_key("kept") && !state.claims.contains_key("lost"));
    assert!(control.changes_after(Generation { epoch: 1, revision: state.revision }).is_none());
    let (position, restored) = (state.position(), state.revision);
    let lost_player = lost.identity.as_ref().unwrap().uuid.clone();
    // The JVM still serves the lost delivery, so no new login is admitted until recovery fences it.
    fixture.runtime.available.store(false, Ordering::Release);
    let relogin = request("relogin", &lost_player);
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));
    fixture.runtime.available.store(true, Ordering::Release);
    control.reconcile_all().await.unwrap();
    {
        let bindings = fixture.runtime.bindings.lock().unwrap();
        assert_eq!(bindings["lost"].phase, DeliveryPhase::Closed);
        assert_eq!(bindings["kept"].phase, DeliveryPhase::Prepared);
    }

    // The lost operation is retired with the generations the JVM holds; retrying it reserves nothing.
    let tombstone = control.state().unwrap().claims["lost"].clone();
    assert!(tombstone.phase == Phase::Released);
    assert_eq!(tombstone.generation, Generation::from_wire(outdated.delivery_generation));
    assert!(control.claim(lost.clone()).await.is_err());
    assert!(control.activate(ActivateClaim { claim: Some(outdated.clone()) }).await.is_err());
    assert!(!control.state().unwrap().players.contains_key(&lost_player));

    // A fresh login reuses the lost claim's revision under the new epoch; the pair tells them apart.
    let fresh = control.claim(relogin).await.unwrap().claim.unwrap();
    let (lost_revision, current) = (outdated.delivery_generation & REVISION_MASK, control.state().unwrap().revision);
    assert!(restored < lost_revision && lost_revision <= current);
    assert!(fresh.delivery_generation > outdated.delivery_generation);
    control.activate(ActivateClaim { claim: Some(fresh) }).await.unwrap();
    assert_eq!(control.inspect(kept).await.unwrap().claim, kept_assignment.claim);

    let changes = control.changes_after(position).unwrap();
    assert!(changes.iter().any(|change| change.table == Table::Claims && change.id == "lost" && !change.removed));
    assert!(changes.iter().all(|change| change.position > position));
    let latest = *control.subscribe().borrow();
    assert_eq!(latest, control.state().unwrap().position());
    assert_eq!(control.changes_after(latest), Some(Vec::new()));
    fixture.close().await;
}

#[tokio::test]
async fn a_surviving_jvm_whose_host_creation_was_lost_is_fenced_before_admission() {
    use chunk_proto::v1::{ProcessRegistration, supervisor_server::Supervisor};

    let fixture = Fixture::new().await;
    let path = fixture.directory.path().join("control.sqlite");
    let storage = Arc::new(Memory::default());
    drop(fixture.control());
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage.clone())).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // The host's creation and its player's admission never reach object storage.
    let control = fixture.control();
    let player = uuid::Uuid::new_v4().to_string();
    control.claim(request("lost", &player)).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["lost"].session].host.clone();
    drop(control);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    drop(SqliteStore::open_replicated(&path, "test", Replication::new(storage)).unwrap());
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control();
    assert!(control.state().unwrap().hosts.is_empty());
    let relogin = request("relogin", &player);
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));

    let service = crate::Service::new(control.clone(), "control-group-credential-with-32-characters".into()).unwrap();
    let mut registration = Request::new(ProcessRegistration {
        identity: Some(ProcessIdentity { runtime_id: host, ..fixture.runtime.identity.clone() }),
        control_endpoint: fixture.host.endpoint.clone(),
        player_endpoint: "127.0.0.1:1".into(),
    });
    registration.metadata_mut().insert("authorization", "Bearer test-runtime-credential".parse().unwrap());
    service.register_process(registration).await.unwrap();
    control.reconcile_all().await.unwrap();
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["lost"].phase, DeliveryPhase::Closed);
    assert!(control.state().unwrap().claims["lost"].phase == Phase::Released);
    control.claim(relogin).await.unwrap();
    fixture.close().await;
}
