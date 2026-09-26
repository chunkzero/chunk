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
        let deployment: Deployment = serde_json::from_str(&contract?).map_err(|_| {
            Error::Corrupt("stored deployment contract is incompatible or corrupt; rebuild with fresh local state")
        })?;
        if deployment.contract_version != chunk_contract::CONTRACT_VERSION {
            return Err(Error::Corrupt(
                "stored deployment contract version is unsupported; rebuild with fresh local state",
            ));
        }
        deployment.validate().map_err(Error::Corrupt)?;
        deployments.push(deployment);
    }
    Ok(deployments)
}

pub(super) fn insert(transaction: &Connection, deployment: &Deployment) -> Result<()> {
    deployment.validate().map_err(Error::Invalid)?;
    if deployment.tables.keys().any(|table| crate::is_system_table(table)) {
        return Err(Error::Invalid("deployment declares a reserved chunk_ table"));
    }
    let encoded = serde_json::to_string(deployment)?;
    let retired: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM _chunk_retired_deployments WHERE id = ?1)",
        [&deployment.id],
        |row| row.get(0),
    )?;
    if retired {
        return Err(Error::Invalid("deployment identity was retired"));
    }
    let existing: Option<String> = transaction
        .query_row("SELECT contract FROM _chunk_deployments WHERE id = ?1", [&deployment.id], |row| row.get(0))
        .optional()?;
    if let Some(existing) = existing {
        return if existing == encoded { Ok(()) } else { Err(Error::Invalid("immutable deployment changed")) };
    }
    let count: i64 = transaction.query_row("SELECT count(*) FROM _chunk_deployments", [], |row| row.get(0))?;
    if count >= 16 {
        return Err(Error::Capacity);
    }
    transaction.execute("INSERT INTO _chunk_deployments VALUES (?1, ?2)", params![deployment.id, encoded])?;
    Ok(())
}

pub(super) fn release(transaction: &Connection, id: &str) -> Result<bool> {
    let referenced: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM _chunk_jobs WHERE deployment=?1 AND state IN ('pending','running'))",
        [id],
        |row| row.get(0),
    )?;
    if referenced {
        return Err(Error::Invalid("deployment has unfinished jobs"));
    }
    let removed = transaction.execute("DELETE FROM _chunk_deployments WHERE id = ?1", [id])? != 0;
    if removed {
        transaction.execute("INSERT INTO _chunk_retired_deployments VALUES (?1)", [id])?;
    }
    Ok(removed)
}
