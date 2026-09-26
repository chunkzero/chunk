use super::*;
use crate::{Generation, Table};
use chunk_store::{ObjectStorage, Replication, SqliteStore};

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
    fn list(&self, prefix: &str) -> std::io::Result<Vec<(String, u64)>> {
        let objects = self.0.lock().unwrap();
        let prefix = format!("{}/", prefix.trim_end_matches('/'));
        Ok(objects
            .iter()
            .filter(|(key, _)| key.starts_with(&prefix))
            .map(|(key, bytes)| (key.clone(), bytes.len() as u64))
            .collect())
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
    let position = state.position();
    // The JVM still holds the lost delivery; the log does not own it, so reconciliation withdraws it.
    control.reconcile_all().await.unwrap();
    {
        let bindings = fixture.runtime.bindings.lock().unwrap();
        assert_eq!(bindings["lost"].phase, DeliveryPhase::Closed);
        assert_eq!(bindings["kept"].phase, DeliveryPhase::Prepared);
    }

    // A retry reserves the same revision under the new epoch; the pair tells the two claims apart.
    let retried = control.claim(lost.clone()).await.unwrap().claim.unwrap();
    assert_eq!(retried.delivery_generation & REVISION_MASK, outdated.delivery_generation & REVISION_MASK);
    assert!(retried.delivery_generation > outdated.delivery_generation);
    assert!(control.activate(ActivateClaim { claim: Some(outdated) }).await.is_err());
    control.activate(ActivateClaim { claim: Some(retried) }).await.unwrap();
    assert_eq!(control.inspect(kept).await.unwrap().claim, kept_assignment.claim);

    let changes = control.changes_after(position).unwrap();
    assert!(changes.iter().any(|change| change.table == Table::Claims && change.id == "lost" && !change.removed));
    assert!(changes.iter().all(|change| change.position > position));
    let latest = *control.subscribe().borrow();
    assert_eq!(latest, control.state().unwrap().position());
    assert_eq!(control.changes_after(latest), Some(Vec::new()));
    fixture.close().await;
}
