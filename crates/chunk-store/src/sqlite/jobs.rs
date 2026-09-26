use chunk_contract::{Deployment, FunctionKind, validate_wire_value};
use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, Job, JobCommand, JobIntent, JobState, Jobs, Result, WakeHandoff};

/// How many job records, of any state, the store keeps and how many bytes they
/// may take. Scheduling beyond either fails with [`Error::JobBudget`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobLimits {
    pub jobs: usize,
    pub bytes: usize,
}

impl Default for JobLimits {
    /// Four retained jobs per player for 5,000 players.
    fn default() -> Self {
        Self { jobs: 20_000, bytes: 64 * 1024 * 1024 }
    }
}

fn state(state: JobState) -> &'static str {
    match state {
        JobState::Pending => "pending",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Failed => "failed",
        JobState::Unknown => "unknown",
        JobState::Cancelled => "cancelled",
    }
}

pub(super) fn load(connection: &Connection) -> Result<Jobs> {
    let mut statement = connection.prepare("SELECT payload FROM _chunk_jobs ORDER BY id")?;
    let values = statement.query_map([], |row| row.get::<_, String>(0))?;
    let records = values.map(|value| Ok(serde_json::from_str(&value?)?)).collect::<Result<Vec<Job>>>()?;
    let running = u32::try_from(records.iter().filter(|job| job.state == JobState::Running).count())
        .map_err(|_| Error::Corrupt("job count"))?;
    let wake = connection.query_row(
        "SELECT generation,next_due,ack_generation=generation FROM _chunk_job_wake WHERE singleton=1",
        [],
        |row| Ok(WakeHandoff { generation: row.get(0)?, due_at: row.get(1)?, acknowledged: row.get(2)?, running }),
    )?;
    Ok(Jobs { records, wake })
}

fn get(connection: &Connection, id: &str) -> Result<Job> {
    let value: Option<String> =
        connection.query_row("SELECT payload FROM _chunk_jobs WHERE id=?1", [id], |row| row.get(0)).optional()?;
    serde_json::from_str(&value.ok_or(Error::Invalid("unknown scheduled job"))?).map_err(Error::from)
}

fn save(connection: &Connection, job: &Job) -> Result<()> {
    let payload = serde_json::to_string(job)?;
    if payload.len() > 256 * 1024 {
        return Err(Error::Capacity);
    }
    connection.execute("INSERT INTO _chunk_jobs (id,deployment,state,due_at,payload,updated_at) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET state=excluded.state,due_at=excluded.due_at,payload=excluded.payload,updated_at=excluded.updated_at",params![job.id,job.deployment,state(job.state),job.due_at,payload,super::retention::now()])?;
    Ok(())
}

fn target(connection: &Connection, job: &Job) -> Result<()> {
    let encoded: Option<String> = connection
        .query_row("SELECT contract FROM _chunk_deployments WHERE id=?1", [&job.deployment], |row| row.get(0))
        .optional()?;
    let deployment: Deployment =
        serde_json::from_str(&encoded.ok_or(Error::Invalid("job deployment is not retained"))?)?;
    let function = deployment.functions.get(&job.function).ok_or(Error::Invalid("unknown job function"))?;
    if function.kind != FunctionKind::Action || !function.arguments.accepts(&job.arguments) {
        return Err(Error::Invalid("job action contract"));
    }
    Ok(())
}

fn due(due: i64) -> Result<()> {
    if !(0..=8_640_000_000_000_000).contains(&due) {
        return Err(Error::Invalid("scheduled timestamp"));
    }
    Ok(())
}

fn totals(connection: &Connection) -> Result<(usize, usize)> {
    // Reserve the longest state name so a full queue can always become terminal.
    connection
        .query_row(
            "SELECT count(*),coalesce(sum(length(CAST(payload AS BLOB))+9-length(state)),0) FROM _chunk_jobs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(Error::from)
}

fn charge(job: &Job) -> Result<usize> {
    Ok(serde_json::to_vec(job)?.len() + 9 - state(job.state).len())
}

pub(super) fn changed(connection: &Connection) -> Result<()> {
    connection.execute("UPDATE _chunk_job_wake SET generation=generation+1,next_due=(SELECT min(due_at) FROM _chunk_jobs WHERE state='pending') WHERE singleton=1",[])?;
    Ok(())
}

pub(super) fn apply(connection: &Connection, intents: &[JobIntent], limits: &JobLimits) -> Result<()> {
    if intents.len() > 16 {
        return Err(Error::Capacity);
    }
    for intent in intents {
        match intent {
            JobIntent::Schedule(job) => {
                if job.id.is_empty()
                    || job.id.len() > 128
                    || job.attempt != 1
                    || job.state != JobState::Pending
                    || job.result.is_some()
                {
                    return Err(Error::Invalid("scheduled job identity"));
                }
                due(job.due_at)?;
                for value in [&job.arguments, &job.caller] {
                    validate_wire_value(value).map_err(Error::Invalid)?;
                    if serde_json::to_vec(value)?.len() > 64 * 1024 {
                        return Err(Error::Capacity);
                    }
                }
                target(connection, job)?;
                let exists: bool =
                    connection.query_row("SELECT EXISTS(SELECT 1 FROM _chunk_jobs WHERE id=?1)", [&job.id], |row| {
                        row.get(0)
                    })?;
                if exists {
                    return Err(Error::Invalid("scheduled job ID reused"));
                }
                save(connection, job)?;
            }
            JobIntent::Cancel { id, caller } => {
                let mut job = get(connection, id)?;
                if &job.caller != caller {
                    return Err(Error::Invalid("job caller mismatch"));
                }
                match job.state {
                    JobState::Pending => job.state = JobState::Cancelled,
                    JobState::Running => job.state = JobState::Unknown,
                    _ => {}
                }
                save(connection, &job)?;
            }
            JobIntent::Retry { id, caller, due_at, acknowledge_possible_effects } => {
                let mut job = get(connection, id)?;
                if &job.caller != caller
                    || !acknowledge_possible_effects
                    || !matches!(job.state, JobState::Failed | JobState::Unknown | JobState::Cancelled)
                {
                    return Err(Error::Invalid(
                        "job retry requires terminal state, owner and possible-effects acknowledgement",
                    ));
                }
                due(*due_at)?;
                target(connection, &job)?;
                job.state = JobState::Pending;
                job.due_at = *due_at;
                job.attempt = job.attempt.checked_add(1).ok_or(Error::Capacity)?;
                job.result = None;
                save(connection, &job)?;
            }
        }
    }
    if intents.iter().any(|intent| matches!(intent, JobIntent::Schedule(_))) {
        let (count, bytes) = totals(connection)?;
        if count > limits.jobs || bytes > limits.bytes {
            return Err(Error::JobBudget);
        }
    }
    if !intents.is_empty() {
        changed(connection)?;
    }
    Ok(())
}

pub(super) fn command(transaction: &Connection, command: JobCommand, limits: &JobLimits) -> Result<Jobs> {
    match command {
        JobCommand::Recover => {
            let jobs = load(transaction)?;
            let mut recovered = false;
            for mut job in jobs.records {
                if job.state == JobState::Running {
                    job.state = JobState::Unknown;
                    save(transaction, &job)?;
                    recovered = true;
                }
            }
            if recovered {
                changed(transaction)?;
            }
        }
        JobCommand::Claim { id, attempt, now } => {
            let mut job = get(transaction, &id)?;
            if job.state != JobState::Pending || job.attempt != attempt || job.due_at > now {
                return Err(Error::Invalid("job is not due"));
            }
            target(transaction, &job)?;
            job.state = JobState::Running;
            save(transaction, &job)?;
            changed(transaction)?;
        }
        JobCommand::Finish { id, attempt, state, result } => {
            if !matches!(state, JobState::Succeeded | JobState::Failed | JobState::Unknown) {
                return Err(Error::Invalid("job completion state"));
            }
            let mut job = get(transaction, &id)?;
            if job.state == JobState::Running && job.attempt == attempt {
                let previous = charge(&job)?;
                if let Some(value) = &result {
                    validate_wire_value(value).map_err(Error::Invalid)?;
                }
                let result_bytes = result.as_ref().map(serde_json::to_vec).transpose()?.map_or(0, |bytes| bytes.len());
                job.state = state;
                job.result = result;
                if result_bytes > 64 * 1024 || totals(transaction)?.1 - previous + charge(&job)? > limits.bytes {
                    job.state = JobState::Failed;
                    job.result = None;
                }
                save(transaction, &job)?;
                changed(transaction)?;
            }
        }
        JobCommand::Forget { id, caller } => {
            let job = get(transaction, &id)?;
            if !job.state.terminal() || job.caller != caller {
                return Err(Error::Invalid("only owner may forget terminal job"));
            }
            transaction.execute("DELETE FROM _chunk_jobs WHERE id=?1", [id])?;
            changed(transaction)?;
        }
        JobCommand::AcknowledgeWake { generation, due_at } => {
            let wake = load(transaction)?.wake;
            if wake.generation != generation || wake.due_at != due_at {
                return Err(Error::Invalid("stale wake acknowledgement"));
            }
            transaction.execute("UPDATE _chunk_job_wake SET ack_generation=?1 WHERE singleton=1", [generation])?;
        }
    }
    load(transaction)
}
