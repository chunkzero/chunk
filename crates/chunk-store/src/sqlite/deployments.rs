use chunk_contract::Deployment;
use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, Result};

pub(super) fn load(connection: &Connection) -> Result<Vec<Deployment>> {
    let mut statement = connection.prepare("SELECT contract FROM _chunk_deployments ORDER BY id")?;
    let contracts = statement.query_map([], |row| row.get::<_, String>(0))?;
    let mut deployments = Vec::new();
    for contract in contracts {
        if deployments.len() == 16 {
            return Err(Error::Corrupt("deployment retention limit"));
        }
        let deployment: Deployment = serde_json::from_str(&contract?)?;
        deployment.validate().map_err(Error::Corrupt)?;
        deployments.push(deployment);
    }
    Ok(deployments)
}

pub(super) fn retain(connection: &mut Connection, deployment: &Deployment) -> Result<()> {
    deployment.validate().map_err(Error::Invalid)?;
    let encoded = serde_json::to_string(deployment)?;
    let transaction = connection.transaction()?;
    let existing: Option<String> = transaction
        .query_row(
            "SELECT contract FROM _chunk_deployments WHERE id = ?1",
            [&deployment.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        return if existing == encoded {
            Ok(())
        } else {
            Err(Error::Invalid("immutable deployment changed"))
        };
    }
    let count: i64 = transaction.query_row("SELECT count(*) FROM _chunk_deployments", [], |row| row.get(0))?;
    if count >= 16 {
        return Err(Error::Capacity);
    }
    transaction.execute(
        "INSERT INTO _chunk_deployments VALUES (?1, ?2)",
        params![deployment.id, encoded],
    )?;
    transaction.commit()?;
    Ok(())
}
