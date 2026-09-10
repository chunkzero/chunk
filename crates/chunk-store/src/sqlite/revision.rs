use rusqlite::{
    Connection, ToSql,
    types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef},
};

use crate::{Error, Result, Revision};

impl FromSql for Revision {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        let raw = value.as_i64()?;
        u64::try_from(raw).map(Self).map_err(|_| FromSqlError::OutOfRange(raw))
    }
}

impl ToSql for Revision {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        i64::try_from(self.0)
            .map(ToSqlOutput::from)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
    }
}

pub(super) fn current(connection: &Connection) -> Result<Revision> {
    Ok(connection.query_row("SELECT revision FROM _chunk_metadata WHERE singleton = 1", [], |row| row.get(0))?)
}

pub(super) fn next(current: Revision) -> Result<Revision> {
    current.0.checked_add(1).filter(|value| *value <= i64::MAX.cast_unsigned()).map(Revision).ok_or(Error::Capacity)
}
