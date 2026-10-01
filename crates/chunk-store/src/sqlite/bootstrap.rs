use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::Connection;

use crate::{Error, Result};

/// Exclusive writer lock beside the database. Dropping it unlocks explicitly:
/// a child forked by another thread shares the open file description until it
/// execs, so closing our descriptor alone would leave the lock held.
pub(super) struct WriterLock(File);

impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

pub(super) fn acquire_writer_lock(path: &Path) -> Result<(PathBuf, WriterLock)> {
    // Create a fresh database file before canonicalizing its lock identity.
    File::options().create(true).truncate(false).write(true).open(path)?;
    let canonical = path.canonicalize()?;
    let mut lock_path = canonical.as_os_str().to_os_string();
    lock_path.push(".writer.lock");
    let writer_lock = File::options().create(true).truncate(false).read(true).write(true).open(lock_path)?;
    writer_lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => Error::WriterLocked,
        std::fs::TryLockError::Error(error) => Error::Io(error),
    })?;
    Ok((canonical, WriterLock(writer_lock)))
}

/// The current store format, recorded as SQLite's `user_version`.
pub(super) const FORMAT: i64 = 10;

pub(crate) fn open(path: &Path, environment: &str) -> Result<Connection> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let version = version(&connection)?;
    if !matches!(version, 0 | 2..=FORMAT) {
        return Err(Error::SchemaVersion(version));
    }
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    if version == 0 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(
            "CREATE TABLE _chunk_metadata (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                environment TEXT NOT NULL,
                revision INTEGER NOT NULL CHECK (revision >= 0),
                document_count INTEGER NOT NULL CHECK (document_count >= 0),
                document_bytes INTEGER NOT NULL CHECK (document_bytes >= 0)
            ) STRICT;
            CREATE TABLE _chunk_operations (
                operation_id TEXT PRIMARY KEY,
                fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
                revision INTEGER NOT NULL UNIQUE CHECK (revision > 0),
                result TEXT NOT NULL
            ) STRICT;
            CREATE TABLE _chunk_migrations (
                revision INTEGER PRIMARY KEY CHECK (revision > 0),
                schema TEXT NOT NULL
            ) STRICT;
            PRAGMA user_version = 2;",
        )?;
        transaction.execute("INSERT INTO _chunk_metadata VALUES (1, ?1, 0, 0, 0)", [environment])?;
        transaction.commit()?;
    }
    let stored: String =
        connection.query_row("SELECT environment FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?;
    if stored != environment {
        return Err(Error::EnvironmentMismatch);
    }
    if (7..FORMAT).contains(&version) {
        // Segments written after a migration cannot apply to a snapshot taken
        // before it. Require a new snapshot before migrating, so a crash at any
        // point either repeats the migration or already left this marker.
        super::log::mark_unlogged(&connection)?;
    }
    upgrade(&connection, version)?;
    Ok(connection)
}

/// Migrates a store of an older `version` to [`FORMAT`], one transaction per format.
fn upgrade(connection: &Connection, version: i64) -> Result<()> {
    if version < 3 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE _chunk_deployments (id TEXT PRIMARY KEY, contract TEXT NOT NULL) STRICT;
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }
    if version < 4 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE _chunk_retry_contexts (
                 operation_id TEXT PRIMARY KEY,
                 fingerprint BLOB NOT NULL CHECK (length(fingerprint) = 32),
                 context TEXT NOT NULL
             ) STRICT;
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
    }
    if version < 5 {
        connection.execute_batch("BEGIN IMMEDIATE; CREATE TABLE _chunk_retired_deployments (id TEXT PRIMARY KEY) STRICT; PRAGMA user_version = 5; COMMIT;")?;
    }
    if version < 6 {
        connection.execute_batch("BEGIN IMMEDIATE;
            CREATE TABLE _chunk_jobs (id TEXT PRIMARY KEY, deployment TEXT NOT NULL, state TEXT NOT NULL, due_at INTEGER NOT NULL, payload TEXT NOT NULL) STRICT;
            CREATE INDEX _chunk_jobs_due ON _chunk_jobs(state,due_at,id);
            CREATE TABLE _chunk_job_wake (singleton INTEGER PRIMARY KEY CHECK(singleton=1),generation INTEGER NOT NULL,ack_generation INTEGER NOT NULL,next_due INTEGER) STRICT;
            INSERT INTO _chunk_job_wake VALUES (1,0,0,NULL);
            PRAGMA user_version = 6; COMMIT;")?;
    }
    if version < 7 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE _chunk_metadata ADD COLUMN epoch INTEGER NOT NULL DEFAULT 1 CHECK (epoch > 0);
             ALTER TABLE _chunk_metadata ADD COLUMN log_sequence INTEGER NOT NULL DEFAULT 0 CHECK (log_sequence >= 0);
             ALTER TABLE _chunk_metadata ADD COLUMN claim TEXT NOT NULL DEFAULT '';
             UPDATE _chunk_metadata SET claim = lower(hex(randomblob(16)));
             CREATE TABLE _chunk_log (sequence INTEGER PRIMARY KEY, entry BLOB NOT NULL) STRICT;
             PRAGMA user_version = 7;
             COMMIT;",
        )?;
    }
    if version < 8 {
        // Existing records get a full retention window from the upgrade.
        let now = super::retention::now();
        connection.execute_batch(&format!(
            "BEGIN IMMEDIATE;
             ALTER TABLE _chunk_operations ADD COLUMN committed_at INTEGER NOT NULL DEFAULT {now};
             CREATE INDEX _chunk_operations_committed ON _chunk_operations(committed_at);
             ALTER TABLE _chunk_retry_contexts ADD COLUMN prepared_at INTEGER NOT NULL DEFAULT {now};
             ALTER TABLE _chunk_jobs ADD COLUMN updated_at INTEGER NOT NULL DEFAULT {now};
             PRAGMA user_version = 8;
             COMMIT;"
        ))?;
    }
    if version < 9 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE _chunk_indexes (definition TEXT PRIMARY KEY) STRICT;
             CREATE TABLE _chunk_work (
                 id INTEGER PRIMARY KEY,
                 work TEXT NOT NULL UNIQUE,
                 done INTEGER NOT NULL CHECK (done >= 0),
                 total INTEGER NOT NULL CHECK (total >= 0)
             ) STRICT;
             PRAGMA user_version = 9;
             COMMIT;",
        )?;
    }
    if version < 10 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             ALTER TABLE _chunk_work ADD COLUMN cursor TEXT;
             CREATE TABLE _chunk_applied (
                 position INTEGER PRIMARY KEY,
                 id TEXT NOT NULL UNIQUE,
                 migration TEXT NOT NULL,
                 active INTEGER NOT NULL CHECK (active IN (0, 1))
             ) STRICT;
             PRAGMA user_version = 10;
             COMMIT;",
        )?;
    }
    Ok(())
}

pub(super) fn version(connection: &Connection) -> Result<i64> {
    Ok(connection.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropping_the_lock_releases_it_while_a_shared_descriptor_survives() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("data.db");
        let (_, lock) = acquire_writer_lock(&path).unwrap();
        // A forked child that has not exec'd yet shares the description like this clone.
        let inherited = lock.0.try_clone().unwrap();
        assert!(matches!(acquire_writer_lock(&path), Err(Error::WriterLocked)));
        drop(lock);
        assert!(acquire_writer_lock(&path).is_ok());
        drop(inherited);
    }
}
