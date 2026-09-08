use std::{
    fs::File,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::Connection;

use crate::{Error, Result};

pub(super) fn acquire_writer_lock(path: &Path) -> Result<(PathBuf, File)> {
    // Create a fresh database file before canonicalizing its lock identity.
    File::options().create(true).truncate(false).write(true).open(path)?;
    let canonical = path.canonicalize()?;
    let mut lock_path = canonical.as_os_str().to_os_string();
    lock_path.push(".writer.lock");
    let writer_lock = File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    writer_lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => Error::WriterLocked,
        std::fs::TryLockError::Error(error) => Error::Io(error),
    })?;
    Ok((canonical, writer_lock))
}

pub(super) fn open(path: &Path, environment: &str) -> Result<Connection> {
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(Duration::from_secs(5))?;
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if !matches!(version, 0 | 2 | 3) {
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
    let stored: String = connection.query_row(
        "SELECT environment FROM _chunk_metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    if stored != environment {
        return Err(Error::EnvironmentMismatch);
    }
    if version != 3 {
        connection.execute_batch(
            "BEGIN IMMEDIATE;
             CREATE TABLE _chunk_deployments (id TEXT PRIMARY KEY, contract TEXT NOT NULL) STRICT;
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }
    Ok(connection)
}
