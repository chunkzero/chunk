use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, Operation, Result, RetryContext};

pub(super) fn prepare(connection: &Connection, operation: &Operation, proposed: RetryContext) -> Result<RetryContext> {
    operation.validate()?;
    if proposed.deployment.is_empty()
        || proposed.deployment.len() > 128
        || !(-8_640_000_000_000_000..=8_640_000_000_000_000).contains(&proposed.timestamp)
    {
        return Err(Error::Invalid("invocation retry context"));
    }
    let existing: Option<(Vec<u8>, String)> = connection
        .query_row(
            "SELECT fingerprint, context FROM _chunk_retry_contexts WHERE operation_id = ?1",
            [&operation.id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((fingerprint, encoded)) = existing {
        let context: RetryContext = serde_json::from_str(&encoded)?;
        if fingerprint != operation.fingerprint || context.deployment != proposed.deployment {
            return Err(Error::OperationMismatch);
        }
        return Ok(context);
    }
    connection.execute(
        "INSERT INTO _chunk_retry_contexts VALUES (?1, ?2, ?3)",
        params![operation.id, operation.fingerprint.as_slice(), serde_json::to_string(&proposed)?],
    )?;
    Ok(proposed)
}
