use super::*;
use crate::Generation;
use chunk_store::{Listed, ObjectStorage, Replication, SqliteStore, Storage};
use prost::Message;

#[tokio::test]
async fn reopens_state_larger_than_the_default_scan_budget() {
    let fixture = Fixture::new();
    let control = fixture.control().await;
    let mut first = request("claim-0", &uuid::Uuid::new_v4().to_string());
    first.identity.as_mut().unwrap().properties.push(chunk_proto::control::v1::Property {
        name: "textures".into(),
        value: "a".repeat(60 * 1024),
        signature: None,
    });
    control.claim(first.clone()).await.unwrap();
    control.cancel(first.clone()).await.unwrap();

    // Batch retained, released claims: base64 requests alone occupy over 40 MiB on disk.
    control
        .update(|state| {
            let template = state.claims["claim-0"].clone();
            let generation = Generation::PENDING;
            for index in 1..512 {
                let mut request = first.clone();
                request.operation_id = format!("claim-{index}");
                request.connection_id = format!("connection-{index}");
                let player = uuid::Uuid::new_v4().to_string();
                request.identity.as_mut().unwrap().uuid.clone_from(&player);
                assert!(request.encoded_len() <= 65_536);
                state.claims.insert(
                    request.operation_id.clone(),
                    crate::state::Claim {
                        request: request.encode_to_vec(),
                        player,
                        membership: generation,
                        generation,
                        assignment: None,
                        ..template.clone()
                    },
                );
            }
            Ok(())
        })
        .unwrap();
    let expected = control.state().unwrap();
    assert_eq!(expected.claims.len(), 512);
    drop(control);
    fixture.detach().await;

    let reopened =
        open(&fixture.directory.path().join("control.sqlite"), fixture.release.clone(), fixture.host.clone())
            .and_then(|control| control.state());
    fixture.close().await;
    let actual = reopened.expect("control must reopen state exceeding the default scan byte budget");
    assert!(actual.claims == expected.claims);
}

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
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("control.sqlite");
    let storage = Arc::new(Memory::default());
    let control = fixture.control().await;
    let kept = request("kept", &uuid::Uuid::new_v4().to_string());
    let kept_assignment = control.claim(kept.clone()).await.unwrap();
    drop(control);
    fixture.detach().await;
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage.clone())).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // These commits never reach object storage before the host is lost.
    let control = fixture.control().await;
    let lost = request("lost", &uuid::Uuid::new_v4().to_string());
    let outdated = control.claim(lost.clone()).await.unwrap().claim.unwrap();
    drop(control);
    fixture.detach().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage)).unwrap();
    assert_eq!(store.epoch().0, 2);
    drop((store, replicator));

    // The JVM still serves the lost delivery, so no new login is admitted until recovery fences it.
    fixture.runtime.available.store(false, Ordering::Release);
    let control = fixture.control().await;
    let state = control.state().unwrap();
    assert_eq!(state.epoch, 2);
    assert!(state.claims.contains_key("kept") && !state.claims.contains_key("lost"));
    assert!(control.authority.feed().after(Generation { epoch: 1, revision: state.revision }).is_none());
    let (position, restored) = (state.position(), state.revision);
    let lost_player = lost.identity.as_ref().unwrap().uuid.clone();
    let relogin = request("relogin", &lost_player);
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));
    fixture.runtime.available.store(true, Ordering::Release);
    fixture.recovered(&control).await;
    {
        let bindings = fixture.runtime.bindings.lock().unwrap();
        assert_eq!(bindings["lost"].phase, JvmDeliveryPhase::Closed);
        assert_eq!(bindings["kept"].phase, JvmDeliveryPhase::Prepared);
    }

    // The lost operation is retired with the generations the JVM holds; retrying it reserves nothing.
    let tombstone = control.state().unwrap().claims["lost"].clone();
    assert!(tombstone.phase == Phase::Released);
    assert_eq!(tombstone.generation, Generation::from_wire(outdated.delivery_generation));
    assert!(control.claim(lost.clone()).await.is_err());
    assert!(control.activate(outdated.clone()).await.is_err());
    assert!(!control.state().unwrap().players.contains_key(&lost_player));

    // A fresh login reuses the lost claim's revision under the new epoch; the pair tells them apart.
    let fresh = control.claim(relogin).await.unwrap().claim.unwrap();
    let (lost_revision, current) = (outdated.delivery_generation & REVISION_MASK, control.state().unwrap().revision);
    assert!(restored < lost_revision && lost_revision <= current);
    assert!(fresh.delivery_generation > outdated.delivery_generation);
    control.activate(fresh).await.unwrap();
    assert_eq!(control.inspect(&kept).unwrap().claim, kept_assignment.claim);

    let (changes, _) = control.authority.feed().after(position).unwrap();
    assert!(changes.iter().any(|change| change.claim == "lost"));
    assert!(changes.iter().all(|change| change.position > position));
    let latest = *control.subscribe().borrow();
    assert_eq!(latest, control.state().unwrap().position());
    assert_eq!(control.authority.feed().after(latest).map(|(changes, _)| changes), Some(Vec::new()));
    fixture.close().await;
}

#[tokio::test]
async fn a_surviving_jvm_whose_host_creation_was_lost_is_fenced_before_admission() {
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("control.sqlite");
    let storage = Arc::new(Memory::default());
    drop(fixture.control().await);
    fixture.detach().await;
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage.clone())).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // The host's creation and its player's admission never reach object storage.
    let control = fixture.control().await;
    let player = uuid::Uuid::new_v4().to_string();
    control.claim(request("lost", &player)).await.unwrap();
    let state = control.state().unwrap();
    let host = state.sessions[&state.claims["lost"].session].host.clone();
    drop(control);
    fixture.detach().await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    drop(SqliteStore::open_replicated(&path, "test", Replication::new(storage)).unwrap());
    fixture.host.forgotten.store(true, Ordering::Release);

    let control = fixture.control().await;
    assert!(control.state().unwrap().hosts.is_empty());
    let relogin = request("relogin", &player);
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));
    control.reconcile_all().await.unwrap();
    assert!(matches!(control.claim(relogin.clone()).await, Err(Error::Busy)));

    control.register_jvm(&host, CREDENTIAL, fixture.host.registration(&host)).unwrap();
    fixture.recovered(&control).await;
    control.admit().unwrap();
    assert_eq!(fixture.runtime.bindings.lock().unwrap()["lost"].phase, JvmDeliveryPhase::Closed);
    let state = control.state().unwrap();
    assert!(state.claims["lost"].phase == Phase::Released);

    // The fenced JVM becomes a retiring host, so the host lifecycle stops and forgets it.
    assert!(state.hosts[&host].retired && state.drains.values().any(|drain| drain.host == host));
    control.shutdown().await.unwrap();
    assert!(fixture.host.terminated.lock().unwrap().contains(&host));
    eventually(|| control.state().unwrap().released(&host)).await;
    control.reconcile_all().await.unwrap();
    let state = control.state().unwrap();
    assert!(!state.hosts.contains_key(&host) && state.drains.is_empty());
    drop(control);
    fixture.control().await.claim(relogin).await.unwrap();
    fixture.close().await;
}

#[tokio::test]
async fn a_session_whose_creation_a_restore_lost_on_a_surviving_host_is_finished_before_admission() {
    let fixture = Fixture::new();
    let path = fixture.directory.path().join("control.sqlite");
    let storage = Arc::new(Memory::default());
    let control = fixture.control().await;
    control.claim(request("kept", &uuid::Uuid::new_v4().to_string())).await.unwrap();
    drop(control);
    fixture.detach().await;
    let (store, replicator) = SqliteStore::open_replicated(&path, "test", Replication::new(storage.clone())).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // The JVM creates a second session on the same host, but the commit recording it is lost.
    let session = uuid::Uuid::new_v4().to_string();
    let running =
        chunk_proto::sync::v1::JvmSession { session_type: "bridge/default".into(), capacity: 2, ..Default::default() };
    fixture.runtime.sessions.lock().unwrap().insert(session.clone(), running);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    drop(SqliteStore::open_replicated(&path, "test", Replication::new(storage)).unwrap());

    // Admission opens once the JVM confirms it ended the session.
    let control = fixture.control().await;
    control.admit().unwrap();
    assert!(fixture.runtime.ended_sessions.lock().unwrap().contains(&session));
    assert!(control.state().unwrap().sessions[&session].finished);
    fixture.close().await;
}
