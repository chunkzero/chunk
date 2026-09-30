use std::{
    collections::BTreeMap,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use rusqlite::{Connection, types::Value};
use serde_json::json;

use super::*;
use crate::{
    Epoch, Error, Operation, RetryContext, Revision, SqliteStore, Storage,
    tests::{commit, job, operation, request, schema, target, write, write_to},
};

#[derive(Default)]
struct Memory(Mutex<BTreeMap<String, (Vec<u8>, SystemTime)>>);

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
        self.0.lock().unwrap().insert(key.into(), (bytes, SystemTime::now()));
        Ok(())
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool> {
        let mut objects = self.0.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        objects.insert(key.into(), (bytes, SystemTime::now()));
        Ok(true)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.0.lock().unwrap().get(key).map(|(bytes, _)| bytes.clone()).ok_or_else(|| io::ErrorKind::NotFound.into())
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Listed>> {
        let objects = self.0.lock().unwrap();
        Ok(objects
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{prefix}/")))
            .map(|(key, (bytes, modified))| Listed { key: key.clone(), size: bytes.len() as u64, modified: *modified })
            .collect())
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// Uploads only on flush, so tests control every object.
fn manual(storage: &Arc<Memory>) -> Replication {
    manual_on(storage.clone())
}

fn manual_on(storage: Arc<dyn ObjectStorage>) -> Replication {
    Replication { batch_delay: Duration::from_secs(3600), ..Replication::new(storage) }
}

fn open(path: &Path, replication: Replication) -> (SqliteStore, Replicator) {
    SqliteStore::open_replicated(path, "local", replication).unwrap()
}

fn epoch_objects(storage: &Memory, epoch: u64) -> Vec<String> {
    storage.keys().into_iter().filter(|key| key.starts_with(&format!("epochs/{epoch:020}/"))).collect()
}

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(5));
    }
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
                .filter(|index| !matches!(columns[*index].as_str(), "epoch" | "claim" | "log_sequence" | "environment"))
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
    // A batch shares one entry; its rejected commit's savepoint rollback must not replicate.
    let rejected = crate::JobIntent::Cancel { id: "missing".into(), caller: json!(null) };
    let results = store.batch(vec![
        request(commit("batch-a", 5, vec![write("f", Some(json!({"coins": 7})))]), vec![]),
        request(commit("batch-rejected", 6, vec![write("g", Some(json!({"coins": 8})))]), vec![rejected]),
        request(commit("batch-b", 6, vec![write("f", None), write("h", Some(json!({"coins": 9})))]), vec![]),
    ]);
    assert!(results[1].is_err() && results[2].is_ok());
    replicator.flush().unwrap();
    let batch_replayed = directory.path().join("batch.db");
    drop(open(&batch_replayed, manual(&storage.copy())));
    assert_eq!(dump(&batch_replayed), dump(&path));
    drop((store, replicator));

    // Writes made without replication reach storage through the next snapshot.
    let mut store = SqliteStore::open(&path, "local").unwrap();
    store.commit(commit("unlogged", 7, vec![write("d", Some(json!({"coins": 4})))])).unwrap();
    drop(store);
    let (mut store, replicator) = open(&path, manual(&storage));
    store.commit(commit("five", 8, vec![write("b", Some(json!({"coins": 5})))])).unwrap();
    replicator.flush().unwrap();
    store.commit(commit("six", 9, vec![write("e", Some(json!({"coins": 6})))])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    assert_eq!((count(&storage, "/snapshots/"), count(&storage, "/segments/")), (3, 4));

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

/// Runs a competing host's actions just before this host's next epoch claim.
struct Racing {
    storage: Arc<Memory>,
    before_claim: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Racing {
    fn new(storage: &Arc<Memory>, before_claim: impl FnOnce() + Send + 'static) -> Arc<Self> {
        Arc::new(Self { storage: storage.clone(), before_claim: Mutex::new(Some(Box::new(before_claim))) })
    }
}

impl ObjectStorage for Racing {
    fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<()> {
        self.storage.put(key, bytes)
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool> {
        let hook = self.before_claim.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        self.storage.create(key, bytes)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.storage.get(key)
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Listed>> {
        self.storage.list(prefix)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.storage.delete(key)
    }
}

type Host = Arc<Mutex<Option<(SqliteStore, Replicator)>>>;

/// Asserts an idle superseded writer learns it was fenced and can no longer commit.
fn assert_fenced(host: &Host, storage: &Memory, id: &str) {
    let (mut store, replicator) = host.lock().unwrap().take().unwrap();
    let keys = storage.keys();
    assert!(!replicator.fenced());
    assert!(matches!(replicator.flush(), Err(Error::Fenced)));
    assert!(replicator.fenced());
    let revision = store.snapshot().unwrap().revision.0;
    assert!(matches!(store.commit(commit(id, revision, vec![])), Err(Error::Fenced)));
    assert_eq!(storage.keys(), keys);
}

#[test]
fn a_restore_that_loses_its_claim_replays_the_winner_s_history() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) = open(&directory.path().join("data.db"), manual(&storage));
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));

    // The winner restores, commits and flushes while the loser is about to claim.
    let winner: Host = Arc::default();
    let hook = {
        let (winner, storage, path) = (winner.clone(), storage.clone(), directory.path().join("winner.db"));
        move || {
            let (mut store, replicator) = open(&path, manual(&storage));
            store.commit(commit("winner", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
            replicator.flush().unwrap();
            *winner.lock().unwrap() = Some((store, replicator));
        }
    };
    let loser = directory.path().join("loser.db");
    let (store, _replicator) = open(&loser, manual_on(Racing::new(&storage, hook)));
    assert_eq!(store.epoch(), Epoch(3));
    assert!(store.outcome(&operation("winner")).unwrap().is_some());
    drop(store);
    assert_fenced(&winner, &storage, "after-takeover");
}

#[test]
fn a_restore_includes_uploads_that_race_its_claim_and_fences_their_writer() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) = open(&directory.path().join("data.db"), manual(&storage));
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();

    // The running writer flushes after the restore listed storage but before it claims.
    let writer: Host = Arc::new(Mutex::new(Some((store, replicator))));
    let hook = {
        let writer = writer.clone();
        move || {
            let mut writer = writer.lock().unwrap();
            let (store, replicator) = writer.as_mut().unwrap();
            store.commit(commit("raced", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
            replicator.flush().unwrap();
        }
    };
    let restored = directory.path().join("restored.db");
    let (store, _replicator) = open(&restored, manual_on(Racing::new(&storage, hook)));
    assert_eq!(store.epoch(), Epoch(3));
    assert!(store.outcome(&operation("raced")).unwrap().is_some());
    drop(store);
    assert_fenced(&writer, &storage, "after-takeover");
}

#[test]
fn idle_writers_detect_a_takeover_and_never_upload_afterwards() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let checked = Replication { fence_interval: Duration::from_millis(10), ..manual(&storage) };
    let (mut idle, idle_replicator) = open(&directory.path().join("idle.db"), checked);
    idle.apply_schema(&schema()).unwrap();
    idle_replicator.flush().unwrap();

    // The idle writer notices the takeover without any commit or flush.
    let (mut taken, taken_replicator) = open(&directory.path().join("taken.db"), manual(&storage));
    assert_eq!(taken.epoch(), Epoch(2));
    eventually("idle writer was never fenced", || idle_replicator.fenced());
    assert!(matches!(idle.commit(commit("idle", 1, vec![])), Err(Error::Fenced)));

    // A commit accepted before the next check is never uploaded.
    drop(open(&directory.path().join("later.db"), manual(&storage)));
    taken.commit(commit("unsent", 1, vec![])).unwrap();
    let keys = storage.keys();
    assert!(matches!(taken_replicator.flush(), Err(Error::Fenced)));
    assert_eq!(storage.keys(), keys);
    assert!(matches!(taken.commit(commit("after", 2, vec![])), Err(Error::Fenced)));
}

#[test]
fn losing_a_new_database_before_its_first_upload_still_allows_a_fresh_start() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) = open(&directory.path().join("lost.db"), manual(&storage));
    assert_eq!(store.epoch(), Epoch(1));
    store.apply_schema(&schema()).unwrap();
    drop((store, replicator));

    let (mut store, replicator) = open(&directory.path().join("fresh.db"), manual(&storage));
    assert_eq!((store.epoch(), store.snapshot().unwrap().revision), (Epoch(2), Revision(0)));
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    let (mut store, _replicator) = open(&directory.path().join("restored.db"), manual(&storage));
    assert_eq!((store.epoch(), store.snapshot().unwrap().revision), (Epoch(3), Revision(1)));
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
fn progress_is_flushed_only_while_every_commit_is_uploaded() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) = open(&directory.path().join("data.db"), manual(&storage));
    replicator.flush().unwrap();
    let progress = replicator.progress();
    assert!(progress.flushed());
    store.apply_schema(&schema()).unwrap();
    assert!(!progress.flushed());
    replicator.flush().unwrap();
    assert!(progress.flushed());
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
fn a_fork_copies_the_latest_state_into_a_new_lineage_without_writing_to_the_source() {
    let source = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("source.db");
    let (mut store, replicator) = open(&path, manual(&source));
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    store.commit(commit("one", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    replicator.flush().unwrap();
    let before = source.keys();

    let target = Arc::new(Memory::default());
    let forked = directory.path().join("fork.db");
    let (mut fork, fork_replicator) =
        SqliteStore::fork(&manual(&source), "local", &forked, "preview", manual(&target), false).unwrap();
    assert_eq!(fork.epoch(), Epoch(1));
    assert_eq!(count(&target, "/snapshots/"), 1);
    fork.commit(commit("fork-only", 2, vec![write("b", Some(json!({"coins": 2})))])).unwrap();
    fork_replicator.flush().unwrap();
    store.commit(commit("source-only", 2, vec![write("c", Some(json!({"coins": 3})))])).unwrap();
    assert_eq!(source.keys(), before);
    drop((fork, fork_replicator));

    let restored = directory.path().join("restored.db");
    let (store, _replicator) = SqliteStore::open_replicated(&restored, "preview", manual(&target)).unwrap();
    assert_eq!(store.epoch(), Epoch(2));
    assert!(store.outcome(&operation("fork-only")).unwrap().is_some());
    assert!(store.outcome(&operation("source-only")).unwrap().is_none());
    drop(store);
    assert_eq!(dump(&restored), dump(&forked));

    assert!(matches!(
        SqliteStore::fork(
            &manual(&source),
            "local",
            directory.path().join("again.db"),
            "preview",
            manual(&target),
            false
        ),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        SqliteStore::fork(
            &manual(&target),
            "local",
            directory.path().join("wrong.db"),
            "other",
            manual(&Arc::default()),
            false
        ),
        Err(Error::EnvironmentMismatch)
    ));
}

#[test]
fn a_fork_drops_retry_contexts_system_rows_and_unfinished_jobs_unless_asked_to_keep_jobs() {
    let source = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) = open(&directory.path().join("source.db"), manual(&source));
    store.apply_schema(&schema()).unwrap();
    let system = serde_json::from_value(json!({"chunk_hosts": {"fields": {"name": {"schema": {"type": "string"}}}}}));
    store.apply_schema(&system.unwrap()).unwrap();
    store.retain_deployment(&target()).unwrap();
    let scheduled = vec![crate::JobIntent::Schedule(job("later"))];
    let writes = vec![write("a", Some(json!({"coins": 1}))), write_to("chunk_hosts", "h", Some(json!({"name": "h"})))];
    store.commit_with_jobs(commit("schedule", 2, writes), scheduled).unwrap();
    let inherited = RetryContext { deployment: "v1".into(), timestamp: 1, seed: 2 };
    store.prepare_operation(&operation("unfinished"), inherited.clone()).unwrap();
    replicator.flush().unwrap();

    let fresh = RetryContext { deployment: "v1".into(), timestamp: 3, seed: 4 };
    let fork = |name: &str, keep_jobs| {
        let (store, _replicator) = SqliteStore::fork(
            &manual(&source),
            "local",
            directory.path().join(name),
            "preview",
            manual(&Arc::default()),
            keep_jobs,
        )
        .unwrap();
        store
    };
    let mut dropped = fork("dropped.db", false);
    let snapshot = dropped.snapshot().unwrap();
    assert!(snapshot.get(&crate::DocumentKey::new("profiles", "a").unwrap()).unwrap().is_some());
    assert!(snapshot.get(&crate::DocumentKey::new("chunk_hosts", "h").unwrap()).unwrap().is_none());
    assert!(dropped.jobs().unwrap().records.is_empty());
    assert!(dropped.outcome(&operation("schedule")).unwrap().is_some());
    assert_eq!(dropped.prepare_operation(&operation("unfinished"), fresh.clone()).unwrap(), fresh);
    let mut kept = fork("kept.db", true);
    assert_eq!(kept.jobs().unwrap().records.iter().map(|job| job.id.as_str()).collect::<Vec<_>>(), ["later"]);
    assert_eq!(kept.prepare_operation(&operation("unfinished"), fresh.clone()).unwrap(), fresh);

    // Failing after the database is installed still leaves no inherited work in it.
    let interrupted = directory.path().join("interrupted.db");
    let failing = Arc::new(FailingLists { storage: Arc::default(), remaining: Mutex::new(1) });
    assert!(SqliteStore::fork(&manual(&source), "local", &interrupted, "preview", manual_on(failing), false).is_err());
    let mut reopened = SqliteStore::open(&interrupted, "preview").unwrap();
    assert!(reopened.jobs().unwrap().records.is_empty());
    assert_eq!(reopened.prepare_operation(&operation("unfinished"), fresh.clone()).unwrap(), fresh);
    assert_eq!(store.prepare_operation(&operation("unfinished"), fresh).unwrap(), inherited);
}

/// Fails every listing after the first `remaining`.
struct FailingLists {
    storage: Arc<Memory>,
    remaining: Mutex<usize>,
}

impl ObjectStorage for FailingLists {
    fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<()> {
        self.storage.put(key, bytes)
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool> {
        self.storage.create(key, bytes)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.storage.get(key)
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Listed>> {
        let mut remaining = self.remaining.lock().unwrap();
        if *remaining == 0 {
            return Err(io::Error::other("listing failed"));
        }
        *remaining -= 1;
        self.storage.list(prefix)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        self.storage.delete(key)
    }
}

#[test]
fn a_crash_right_after_a_format_migration_still_forces_a_new_snapshot() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let (mut store, replicator) = open(&path, manual(&storage));
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    // A format-7 database as an older binary left it, migrated by a process
    // that stops before replication resumes.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "DROP INDEX _chunk_operations_committed;
             ALTER TABLE _chunk_operations DROP COLUMN committed_at;
             ALTER TABLE _chunk_retry_contexts DROP COLUMN prepared_at;
             ALTER TABLE _chunk_jobs DROP COLUMN updated_at;
             PRAGMA user_version = 7;",
        )
        .unwrap();
    drop(crate::sqlite::bootstrap::open(&path, "local").unwrap());

    let snapshots = count(&storage, "/snapshots/");
    let (mut store, replicator) = open(&path, manual(&storage));
    store.commit(commit("after", 1, vec![])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    // The migrated state reaches storage as a snapshot, never as segments on the old one.
    assert_eq!(count(&storage, "/snapshots/"), snapshots + 1);
    let (store, _replicator) = open(&directory.path().join("restored.db"), manual(&storage));
    assert!(store.outcome(&operation("after")).unwrap().is_some());
}

#[test]
fn format_7_snapshots_and_segments_restore_and_fork_before_migrating() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    // A format-7 host's objects: a snapshot at sequence 1, then one logged commit.
    let legacy = directory.path().join("legacy.db");
    let mut store = SqliteStore::open(&legacy, "local").unwrap();
    store.apply_schema(&schema()).unwrap();
    store.commit(commit("one", 1, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    drop(store);
    let connection = Connection::open(&legacy).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode = DELETE;
             DROP INDEX _chunk_operations_committed;
             ALTER TABLE _chunk_operations DROP COLUMN committed_at;
             ALTER TABLE _chunk_retry_contexts DROP COLUMN prepared_at;
             ALTER TABLE _chunk_jobs DROP COLUMN updated_at;
             DELETE FROM _chunk_log;
             UPDATE _chunk_metadata SET epoch = 1, log_sequence = 1;
             PRAGMA user_version = 7;",
        )
        .unwrap();
    storage.create(&segment::claim_key(1), b"legacy".to_vec()).unwrap();
    storage.put(&segment::snapshot_key(1, 1), std::fs::read(&legacy).unwrap()).unwrap();
    let mut session = rusqlite::session::Session::new(&connection).unwrap();
    session.attach(None::<&str>).unwrap();
    connection
        .execute_batch(
            "UPDATE _chunk_metadata SET revision = 3, log_sequence = 2;
             INSERT INTO _chunk_operations SELECT 'two', fingerprint, 3, 'null' FROM _chunk_operations;",
        )
        .unwrap();
    let mut changeset = Vec::new();
    session.changeset_strm(&mut changeset).unwrap();
    drop(session);
    let entry = Entry { sequence: 2, revision: 3, statements: Vec::new(), changeset }.encode().unwrap();
    storage.put(&segment::segment_key(1, 2, 2), segment::encode(1, [entry.as_slice()]).unwrap()).unwrap();

    let (fork, _replicator) = SqliteStore::fork(
        &manual(&storage),
        "local",
        directory.path().join("fork.db"),
        "preview",
        manual(&Arc::default()),
        false,
    )
    .unwrap();
    assert!(fork.outcome(&operation("two")).unwrap().is_some());

    let (mut store, replicator) = open(&directory.path().join("restored.db"), manual(&storage));
    assert_eq!((store.epoch(), store.snapshot().unwrap().revision), (Epoch(2), Revision(3)));
    assert!(store.outcome(&operation("one")).unwrap().is_some() && store.outcome(&operation("two")).unwrap().is_some());
    store.commit(commit("three", 3, vec![])).unwrap();
    replicator.flush().unwrap();
    drop((store, replicator));
    // The next restore starts from the migrated epoch's snapshot.
    let (store, _replicator) = open(&directory.path().join("again.db"), manual(&storage));
    assert_eq!((store.epoch(), store.outcome(&operation("three")).unwrap().is_some()), (Epoch(3), true));
}

#[test]
fn superseded_objects_are_deleted_once_a_newer_snapshot_is_old_enough() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("data.db");
    let replication = || Replication { snapshot_segments: 1, ..manual(&storage) }.with_retention(Duration::ZERO);
    let (mut store, replicator) = open(&path, replication());
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    for (index, id) in ["one", "two", "three"].into_iter().enumerate() {
        store.commit(commit(id, index as u64 + 1, vec![write(id, Some(json!({"coins": 1})))])).unwrap();
        replicator.flush().unwrap();
    }
    eventually("superseded objects were never pruned", || {
        (count(&storage, "/snapshots/"), count(&storage, "/segments/")) == (1, 1)
    });
    drop((store, replicator));

    let restored = directory.path().join("restored.db");
    let (mut store, replicator) = open(&restored, replication());
    store.commit(commit("four", 4, vec![])).unwrap();
    replicator.flush().unwrap();
    // The new epoch's first snapshot supersedes every object of epoch 1 except its claim.
    eventually("epoch 1 was never pruned", || epoch_objects(&storage, 1) == [segment::claim_key(1)]);
    drop((store, replicator));
    let (store, _replicator) = open(&directory.path().join("again.db"), manual(&storage));
    assert!(store.outcome(&operation("four")).unwrap().is_some());
    drop(store);
    assert_eq!(dump(&directory.path().join("again.db")), dump(&restored));

    // Within the window nothing is deleted.
    let kept = Arc::new(Memory::default());
    let (mut store, replicator) =
        open(&directory.path().join("kept.db"), Replication { snapshot_segments: 1, ..manual(&kept) });
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    store.commit(commit("one", 1, vec![])).unwrap();
    replicator.flush().unwrap();
    store.commit(commit("two", 2, vec![])).unwrap();
    replicator.flush().unwrap();
    assert_eq!(count(&kept, "/snapshots/"), 2);
}

/// Takes `delay` for every delete, like a slow bucket.
struct SlowDeletes(Arc<Memory>, Duration);

impl ObjectStorage for SlowDeletes {
    fn put(&self, key: &str, bytes: Vec<u8>) -> io::Result<()> {
        self.0.put(key, bytes)
    }

    fn create(&self, key: &str, bytes: Vec<u8>) -> io::Result<bool> {
        self.0.create(key, bytes)
    }

    fn get(&self, key: &str) -> io::Result<Vec<u8>> {
        self.0.get(key)
    }

    fn list(&self, prefix: &str) -> io::Result<Vec<Listed>> {
        self.0.list(prefix)
    }

    fn delete(&self, key: &str) -> io::Result<()> {
        std::thread::sleep(self.1);
        self.0.delete(key)
    }
}

#[test]
fn a_deletion_backlog_never_holds_up_uploads_or_fencing() {
    let storage = Arc::new(Memory::default());
    let directory = tempfile::tempdir().unwrap();
    let (mut store, replicator) =
        open(&directory.path().join("data.db"), Replication { snapshot_segments: 1, ..manual(&storage) });
    store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();
    for revision in 1..=20 {
        store.commit(commit(&format!("old-{revision}"), revision, vec![])).unwrap();
        replicator.flush().unwrap();
    }
    drop((store, replicator));
    let backlog = epoch_objects(&storage, 1).len();
    assert!(backlog > 20);

    // Deleting the backlog takes seconds; uploads and fence checks go between deletes.
    let slow = Arc::new(SlowDeletes(storage.clone(), Duration::from_millis(200)));
    let replication =
        Replication { fence_interval: Duration::from_millis(10), ..manual_on(slow) }.with_retention(Duration::ZERO);
    let (mut store, replicator) = open(&directory.path().join("restored.db"), replication);
    assert_eq!(store.epoch(), Epoch(2));
    store.commit(commit("new", 21, vec![])).unwrap();
    replicator.flush().unwrap();
    assert!(epoch_objects(&storage, 1).len() > backlog / 2, "the upload waited for the backlog");
    eventually("deletions stopped after the upload", || epoch_objects(&storage, 1).len() < backlog);

    assert!(storage.create(&segment::claim_key(3), Vec::new()).unwrap());
    eventually("the writer was never fenced", || replicator.fenced());
    assert!(epoch_objects(&storage, 1).len() > backlog / 2, "fencing waited for the backlog");
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
    let listed = replication.storage().list("probes").unwrap();
    assert!(listed.iter().any(|object| object.key == claim
        && SystemTime::now().duration_since(object.modified).unwrap_or_default() < Duration::from_secs(600)));
    replication.storage().delete(claim).unwrap();
    replication.storage().delete(claim).unwrap();
    assert!(replication.storage().list("probes").unwrap().is_empty());
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

/// Run like [`s3_round_trip`].
#[test]
#[ignore = "needs an S3-compatible server"]
fn s3_credentials_rotate_without_reopening() {
    let directory = tempfile::tempdir().unwrap();
    let mut bucket = s3::S3Bucket::from_env().expect("CHUNK_REPLICATION_BUCKET");
    let unique = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos();
    bucket.prefix = Some(format!("{}/rotation-{unique}", bucket.prefix.unwrap_or_default()));
    let (good, wrong) =
        (s3::S3Credentials::from_env().unwrap(), s3::S3Credentials::new("wrong".into(), "wrong".into(), None));
    let credentials = s3::S3Credentials::new(String::new(), String::new(), None);
    credentials.replace(&good);
    let replication = Replication::s3(&bucket, credentials.clone()).unwrap();
    let (mut store, replicator) =
        open(&directory.path().join("data.db"), Replication { batch_delay: Duration::from_secs(3600), ..replication });
    let revision = store.apply_schema(&schema()).unwrap();
    replicator.flush().unwrap();

    credentials.replace(&wrong);
    store.commit(commit("rotated", revision.0, vec![write("a", Some(json!({"coins": 1})))])).unwrap();
    assert!(matches!(replicator.flush(), Err(Error::Replication(_))));
    credentials.replace(&good);
    replicator.flush().unwrap();
}
