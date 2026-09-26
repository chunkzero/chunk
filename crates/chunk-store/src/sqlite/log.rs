//! Captures write transactions as replayable log entries.

use std::sync::Arc;

use rusqlite::{
    Connection, Transaction, TransactionBehavior, params,
    session::{ConflictAction, Session},
};

use crate::{
    Error, Result,
    replication::{Entry, Shared},
};

/// The first store format with a replicated log.
const REPLICATED_FORMAT: i64 = 7;

/// Present while the store is replicated.
pub(super) struct Log {
    shared: Arc<Shared>,
    pruned: u64,
}

impl Log {
    pub fn new(shared: Arc<Shared>, pruned: u64) -> Self {
        Self { shared, pruned }
    }
}

/// Runs `change` in an immediate transaction. With a log, a transaction that
/// changes rows also appends one entry holding `statements` (schema DDL, which
/// changesets cannot carry) and the changeset of every other table.
pub(super) fn write<T>(
    connection: &Connection,
    log: Option<&mut Log>,
    statements: &[String],
    change: impl FnOnce(&Transaction<'_>) -> Result<T>,
) -> Result<T> {
    if log.as_ref().is_some_and(|log| log.shared.fenced()) {
        return Err(Error::Fenced);
    }
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    let Some(log) = log else {
        let value = change(&transaction)?;
        transaction.commit()?;
        return Ok(value);
    };
    let mut session = Session::new(connection)?;
    session.table_filter(Some(|table: &str| table != "_chunk_log"));
    session.attach(None::<&str>)?;
    let value = change(&transaction)?;
    if session.is_empty() && statements.is_empty() {
        drop(session);
        transaction.commit()?;
        return Ok(value);
    }
    transaction.execute("UPDATE _chunk_metadata SET log_sequence = log_sequence + 1 WHERE singleton = 1", [])?;
    let (sequence, revision) = position(&transaction)?;
    let mut changeset = Vec::new();
    session.changeset_strm(&mut changeset)?;
    drop(session);
    let entry = Entry { sequence, revision, statements: statements.to_vec(), changeset }.encode()?;
    transaction.execute("INSERT INTO _chunk_log VALUES (?1, ?2)", params![sequence, entry])?;
    let uploaded = log.shared.uploaded();
    if uploaded > log.pruned {
        transaction.execute("DELETE FROM _chunk_log WHERE sequence <= ?1", [uploaded])?;
    }
    transaction.commit()?;
    log.pruned = log.pruned.max(uploaded);
    log.shared.committed(sequence, entry.len());
    Ok(value)
}

/// The last assigned log sequence and the environment revision.
pub(crate) fn position(connection: &Connection) -> Result<(u64, u64)> {
    Ok(connection.query_row("SELECT log_sequence, revision FROM _chunk_metadata WHERE singleton = 1", [], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?)
}

pub(crate) fn epoch(connection: &Connection) -> Result<u64> {
    Ok(connection.query_row("SELECT epoch FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?)
}

/// The random token this database wrote to its epoch's claim object.
pub(crate) fn claim(connection: &Connection) -> Result<String> {
    Ok(connection.query_row("SELECT claim FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?)
}

/// Marks changes the replicated log may lack, once replication has been used.
pub(super) fn mark_unlogged(connection: &Connection) -> Result<()> {
    if position(connection)?.0 > 0 {
        skip_sequence(connection)?;
    }
    Ok(())
}

/// Advances the sequence without an entry and drops pending entries, so the
/// uploader sees a gap and replaces the unlogged history with a snapshot.
pub(super) fn skip_sequence(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "BEGIN IMMEDIATE;
         UPDATE _chunk_metadata SET log_sequence = log_sequence + 1 WHERE singleton = 1;
         DELETE FROM _chunk_log;
         COMMIT;",
    )?;
    Ok(())
}

/// Applies one entry to a copy that holds exactly the state before it.
pub(crate) fn replay(connection: &Connection, entry: &Entry) -> Result<()> {
    for statement in &entry.statements {
        connection.execute_batch(statement)?;
    }
    connection
        .apply_strm(&mut entry.changeset.as_slice(), None::<fn(&str) -> bool>, |_, _| {
            ConflictAction::SQLITE_CHANGESET_ABORT
        })
        .map_err(|_| Error::Corrupt("replicated log entry does not apply"))?;
    if position(connection)? != (entry.sequence, entry.revision) {
        return Err(Error::Corrupt("replicated log entry does not match its position"));
    }
    Ok(())
}

/// A downloaded snapshot being brought forward by replayed entries in one transaction.
pub(crate) struct Replica {
    connection: Connection,
    sequence: u64,
}

impl Replica {
    /// Accepts snapshots of any replicated store format. Their segments were
    /// written in the same format, so they replay first; the restored store
    /// migrates when it opens.
    pub fn open(path: &std::path::Path, environment: &str, epoch: u64, sequence: u64) -> Result<Self> {
        let connection = Connection::open(path)?;
        if !(REPLICATED_FORMAT..=super::bootstrap::FORMAT).contains(&super::bootstrap::version(&connection)?) {
            return Err(Error::Corrupt("snapshot has an unsupported store format"));
        }
        let stored: String =
            connection
                .query_row("SELECT environment FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?;
        if stored != environment {
            return Err(Error::EnvironmentMismatch);
        }
        if self::epoch(&connection)? != epoch || position(&connection)?.0 != sequence {
            return Err(Error::Corrupt("snapshot does not match its object key"));
        }
        connection.execute_batch("PRAGMA journal_mode = DELETE; BEGIN IMMEDIATE;")?;
        Ok(Self { connection, sequence })
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn replay(&mut self, entry: &Entry) -> Result<()> {
        if entry.sequence != self.sequence + 1 {
            return Err(Error::Corrupt("replicated log has a gap"));
        }
        replay(&self.connection, entry)?;
        self.sequence = entry.sequence;
        Ok(())
    }

    /// A fresh random claim token.
    pub fn token(&self) -> Result<String> {
        Ok(self.connection.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?)
    }

    /// Drops the source's retry contexts, its system table rows and, unless `keep_jobs`,
    /// its pending and running jobs, so a fork never resumes work or state the source owned.
    pub fn discard_inherited(&self, keep_jobs: bool) -> Result<()> {
        self.connection.execute("DELETE FROM _chunk_retry_contexts", [])?;
        let tables = self
            .connection
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?
            .query_map([], |row| row.get::<_, String>(0))?
            .filter(|name| name.as_ref().is_ok_and(|name| crate::is_system_table(name)))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for table in tables {
            let table = super::codec::quote(&table);
            self.connection.execute(
                &format!(
                    "UPDATE _chunk_metadata SET document_count = document_count - (SELECT count(*) FROM {table}), \
                     document_bytes = document_bytes - (SELECT coalesce(sum(_bytes), 0) FROM {table}) WHERE singleton = 1"
                ),
                [],
            )?;
            self.connection.execute(&format!("DELETE FROM {table}"), [])?;
        }
        if !keep_jobs
            && self.connection.execute("DELETE FROM _chunk_jobs WHERE state IN ('pending', 'running')", [])? > 0
        {
            super::jobs::changed(&self.connection)?;
        }
        Ok(())
    }

    /// Commits the replayed entries as `environment` under a claimed `epoch`,
    /// leaving a single durable file.
    pub fn finish(self, epoch: u64, claim: &str, environment: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE _chunk_metadata SET epoch = ?1, claim = ?2, environment = ?3 WHERE singleton = 1",
            rusqlite::params![epoch, claim, environment],
        )?;
        self.connection.execute_batch("DELETE FROM _chunk_log; COMMIT;")?;
        self.connection.close().map_err(|(_, error)| error)?;
        Ok(())
    }
}
