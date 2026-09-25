use std::{
    collections::BTreeMap,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rusqlite::{Connection, types::Value};
use serde_json::json;

use super::*;
use crate::{
    Epoch, Error, Operation, RetryContext, Revision, SqliteStore, Storage,
    tests::{commit, operation, schema, write},
};

#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, Vec<u8>>>);

impl Memory {
    fn keys(&self) -> Vec<String> {
        self.0.lock().unwrap().keys().cloned().collect()
    }

    fn copy(&self) -> Arc<Self> {
        Arc::new(Self(Mutex::new(self.0.lock().unwrap().clone())))
    }
}

impl ObjectStorage for Memory {
    fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<()> {
        self.0.lock().unwrap().insert(key.into(), bytes);
        Ok(())
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool> {
        let mut objects = self.0.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        objects.insert(key.into(), bytes);
        Ok(true)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).cloned().ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<(String, u64)>> {
        let objects = self.0.lock().unwrap();
        Ok(objects
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{prefix}/")))
            .map(|(key, bytes)| (key.clone(), bytes.len() as u64))
            .collect())
    }
}

/// Uploads only on flush, so tests control every object.
fn manual(storage: &Arc<Memory>) -> Replication {
    Replication { batch_delay: Duration::from_secs(3600), ..Replication::new(storage.clone()) }
}

fn open(path: &Path, replication: Replication) -> (SqliteStore, Replicator) {
    SqliteStore::open_replicated(path, "local", replication).unwrap()
}

fn count(storage: &Memory, kind: &str) -> usize {
    storage.keys().iter().filter(|key| key.contains(kind)).count()
}

/// Every schema object and row except replication bookkeeping.
fn dump(path: &Path) -> Vec<String> {
    let connection = Connection::open(path).unwrap();
    let mut lines = Vec::new();
    let mut tables = connection.prepare("SELECT type, name, sql FROM sqlite_schema ORDER BY name").unwrap();
    let tables: Vec<(String, String, Option<String>)> = tables
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for (kind, name, sql) in tables {
        lines.push(format!("{kind} {name}: {sql:?}"));
        if kind != "table" || name == "_chunk_log" {
            continue;
        }
        let mut rows = connection.prepare(&format!("SELECT * FROM \"{name}\" ORDER BY 1")).unwrap();
        let columns: Vec<String> = rows.column_names().into_iter().map(Into::into).collect();
        let mut cursor = rows.query([]).unwrap();
        while let Some(row) = cursor.next().unwrap() {
            let values: Vec<String> = (0..columns.len())
                .filter(|index| !matches!(columns[*index].as_str(), "epoch" | "claim" | "log_sequence"))
                .map(|index| format!("{:?}", row.get::<_, Value>(index).unwrap()))
                .collect();
            lines.push(format!("  {}", values.join(", ")));
        }
    }
    lines
}

#[test]
fn restore_replays_the_latest_snapshot_and_later_segments_exactly() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let (mut store, replicator) = open(&path, Replication { snapshot_segments: 2, ..manual(&storage) });
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    store
        .commit(commit("one", 1, vec![write("a", Some(json!({"coins": 1}))), write("b", Some(json!({"coins": 2})))]))
        .unwrap();
    replicator.flush().unwrap();
    let extended = serde_json::from_value(json!({"profiles": {
        "fields": {"coins": {"schema": {"type": "integer"}}, "name": {"schema": {"type": "string"}, "optional": true}},
        "indexes": {"by_name": ["name"]}
    }}))
    .unwrap();
    store.apply_schema(&extended).unwrap();
    store.commit(commit("three", 3, vec![write("c", Some(json!({"coins": 3, "name": "c"})))])).unwrap();
    replicator.flush().unwrap();
    assert_eq!((count(&storage, "/snapshots/"), count(&storage, "/segments/")), (1, 2));

    // The schema change is replayed from a segment, not carried by a snapshot.
    let schema_replayed = directory.path().join("schema.db");
    drop(open(&schema_replayed, manual(&storage.copy())));
    assert_eq!(dump(&schema_replayed), dump(&path));

    let context = RetryContext { deployment: "v1".into(), timestamp: 1, seed: 2 };
    store.prepare_operation(&operation("pending"), context).unwrap();
    store.commit(commit("two", 4, vec![write("a", None)])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // Writes made without replication reach storage through the next snapshot.
    let mut store = SqliteStore::open(&path, "local").unwrap();
    store.commit(commit("unlogged", 5, vec![write("d", Some(json!({"coins": 4})))])).unwrap();
    drop(store);
    let (mut store, replicator) = open(&path, manual(&storage));
    store.commit(commit("five", 6, vec![write("b", Some(json!({"coins": 5})))])).unwrap();
    replicator.flush().unwrap();
    store.commit(commit("six", 7, vec![write("e", Some(json!({"coins": 6})))])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    assert_eq!((count(&storage, "/snapshots/"), count(&storage, "/segments/")), (3, 3));

    let restored = directory.path().join("restored.db");
    let (store, _replicator) = open(&restored, manual(&storage));
    assert_eq!(store.epoch(), Epoch(2));
    drop(store);
    assert_eq!(dump(&restored), dump(&path));
}

#[test]
fn successive_crashes_lose_only_unsent_tails_and_never_reuse_a_generation() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let (mut store, replicator) = open(&path, manual(&storage));
    assert_eq!(store.epoch(), Epoch(1));
    store.apply_schema(&schema()).unwrap();
    store.commit(commit("kept", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    replicator.flush().unwrap();
    let lost = store.commit(commit("lost", 2, vec![write("b", Some(json!({"coins": 2})))])).unwrap();
    // Stopping without a flush stands in for a killed process.
    drop((store, replicator));

    let mut generations = vec![(Epoch(1), lost.revision)];
    for (attempt, epoch) in [(2, Epoch(2)), (3, Epoch(3))] {
        let (mut store, replicator) = open(&directory.path().join(format!("{attempt}.db")), manual(&storage));
        assert_eq!(store.epoch(), epoch);
        assert!(store.outcome(&operation("kept")).unwrap().is_some());
        assert!(store.outcome(&operation("lost")).unwrap().is_none());
        let next = store.commit(commit(&format!("next-{attempt}"), 2, vec![])).unwrap();
        assert_eq!(next.revision, lost.revision);
        assert!(!generations.contains(&(store.epoch(), next.revision)));
        generations.push((store.epoch(), next.revision));
        // Acknowledged, then lost before its epoch's first snapshot was uploaded.
        drop((store, replicator));
    }

    assert!(matches!(SqliteStore::open_replicated(&path, "local", manual(&storage)), Err(Error::StaleReplica)));
    let (mut store, replicator) = open(&directory.path().join("last.db"), manual(&storage));
    assert_eq!(store.epoch(), Epoch(4));
    store.commit(commit("durable", 2, vec![])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    let (mut store, _replicator) = open(&directory.path().join("after.db"), manual(&storage));
    assert_eq!(store.epoch(), Epoch(5));
    assert!(store.outcome(&operation("durable")).unwrap().is_some());
    assert_eq!(store.snapshot().unwrap().revision, Revision(3));
}

#[test]
fn competing_restores_claim_distinct_epochs() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) = open(&directory.path().join("data.db"), manual(&storage));
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // Both restores list storage before either claims an epoch.
    let remote = Remote::load(storage.as_ref()).unwrap();
    let epochs: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|name| {
            let path = directory.path().join(name);
            std::fs::File::create(&path).unwrap();
            restore(&path, "local", storage.as_ref(), &remote).unwrap();
            let (store, _replicator) = open(&path, manual(&storage));
            store.epoch()
        })
        .collect();
    assert_eq!(epochs, [Epoch(2), Epoch(3)]);
    assert!(matches!(
        SqliteStore::open_replicated(directory.path().join("first"), "local", manual(&storage)),
        Err(Error::StaleReplica)
    ));
}

#[test]
fn enabling_replication_snapshots_existing_data_before_any_commit() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let mut store = SqliteStore::open(&path, "local").unwrap();
    assert_eq!(store.apply_schema(&schema()).unwrap(), Revision(1));
    drop(store);
    let (_store, replicator) = open(&path, manual(&storage));
    replicator.flush().unwrap();
    assert_eq!(count(&storage, "/snapshots/"), 1);
    let (mut store, _replicator) = open(&directory.path().join("restored.db"), manual(&storage));
    assert_eq!(store.snapshot().unwrap().schema(), &schema());
}

#[cfg(unix)]
#[test]
fn restore_keeps_the_database_private() {
    use std::os::unix::fs::PermissionsExt;
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (_store, replicator) = open(&directory.path().join("data.db"), manual(&storage));
    replicator.flush().unwrap();
    let path = directory.path().join("restored.db");
    std::fs::File::create(&path).unwrap().set_permissions(std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(open(&path, manual(&storage)));
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
}

#[test]
fn uploads_follow_commits_and_an_idle_store_uploads_nothing() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let replication = || Replication { batch_delay: Duration::from_millis(10), ..Replication::new(storage.clone()) };
    let (mut store, replicator) = open(&path, replication());
    replicator.flush().unwrap();
    let initial = storage.keys();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(storage.keys(), initial);

    store.apply_schema(&schema()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while storage.keys() == initial {
        assert!(Instant::now() < deadline, "commit was not uploaded");
        std::thread::sleep(Duration::from_millis(10));
    }
    replicator.flush().unwrap();
    let uploaded = storage.keys();
    std::thread::sleep(Duration::from_millis(100));
    drop((store, replicator));
    let (_store, _replicator) = open(&path, replication());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(storage.keys(), uploaded);
}

#[test]
fn segments_round_trip_and_reject_corruption() {
    let entry = segment::Entry {
        sequence: 7,
        revision: 3,
        statements: vec!["CREATE TABLE t (x)".into()],
        changeset: vec![1, 2],
    };
    let encoded = entry.encode().unwrap();
    let mut bytes = segment::encode(2, [encoded.as_slice()]).unwrap();
    assert_eq!(segment::decode(2, &bytes).unwrap(), vec![entry]);
    assert!(segment::decode(3, &bytes).is_err());
    *bytes.last_mut().unwrap() ^= 1;
    assert!(matches!(segment::decode(2, &bytes), Err(Error::Corrupt(_))));
}

/// Run against a disposable bucket with `CHUNK_REPLICATION_*` set, for example `MinIO`.
#[test]
#[ignore = "needs an S3-compatible server"]
fn s3_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let replication = Replication::from_env().unwrap().expect("CHUNK_REPLICATION_BUCKET");
    let claim = "probes/claim";
    assert!(replication.storage().create(claim, vec![1]).unwrap());
    assert!(!replication.storage().create(claim, vec![2]).unwrap());
    let (mut store, replicator) = open(&path, Replication::from_env().unwrap().unwrap());
    let epoch = store.epoch();
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    let revision = store.snapshot().unwrap().revision;
    store.commit(commit("s3", revision.0, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    let restored = directory.path().join("restored.db");
    let (store, _replicator) = open(&restored, Replication::from_env().unwrap().unwrap());
    assert_eq!(store.epoch(), Epoch(epoch.0 + 1));
    assert!(store.outcome(&Operation { id: "s3".into(), fingerprint: [7; 32] }).unwrap().is_some());
    drop(store);
    assert_eq!(dump(&restored), dump(&path));
}
