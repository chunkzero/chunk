//! Runs migration transforms in a deployment's bundle, one engine invocation per row. Every invocation has the same
//! seed and `Date.now()` returns 0, and a transform can't read, so a row's result never depends on which rows
//! were transformed before it, nor on whether it was transformed in a backfill or by a write.

use std::time::{Duration, Instant};

use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Key, Mode, ReadHost};
use chunk_store::{Backfill, MAX_DOCUMENT_BYTES};
use serde_json::{Value, json};

use crate::Error;

/// The most JSON one invocation takes or returns: a maximum-size document, plus room for its `_id` and the call's
/// envelope.
const INVOCATION_BYTES: usize = MAX_DOCUMENT_BYTES + 4096;

/// The longest the transforms of one backfill batch may take in all.
const BATCH_DEADLINE: Duration = Duration::from_secs(5);
/// The most JSON the transforms of one backfill batch may return in all.
const BATCH_OUTPUT_BYTES: usize = 8 * 1024 * 1024;

/// Which transform of a migration runs.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    To,
    Back,
}

/// Maps `row` through `migration`'s transform for `table` in the bundle of the resident deployment `id`.
pub(crate) fn transform(
    engine: &mut Engine,
    id: &DeploymentId,
    (migration, table, direction): (&str, &str, Direction),
    row: &Value,
    cancellation: &Cancellation,
) -> std::result::Result<Value, String> {
    let direction = match direction {
        Direction::To => "to",
        Direction::Back => "back",
    };
    let arguments = json!({"migration": migration, "table": table, "direction": direction, "rows": [row]});
    let invocation = Invocation {
        export: "__chunk_migrate".into(),
        arguments: arguments.into(),
        caller: Value::Null.into(),
        mode: Mode::Query,
        timestamp: 0,
        seed: 0,
    };
    let execution = engine
        .execute_sized(id, invocation, Box::new(NoReads), cancellation, INVOCATION_BYTES)
        .map_err(|error| error.to_string())?;
    let mut rows: Vec<Value> = serde_json::from_str(&execution.value).map_err(|error| error.to_string())?;
    match (rows.pop(), rows.is_empty()) {
        (Some(row), true) => Ok(row),
        _ => Err("the transform must return one row".into()),
    }
}

struct NoReads;

impl ReadHost for NoReads {
    fn get(&mut self, _: &Key) -> Result<Option<Value>, String> {
        Err("migrations can't read documents".into())
    }

    fn scan(&mut self, _: &str, _: Option<&str>, _: Option<&str>) -> Result<Vec<(String, Value)>, String> {
        Err("migrations can't read documents".into())
    }
}

/// Transforms the rows of `batch` with `to` in the resident deployment `id`. A batch that outlasts the deadline or
/// returns more than the output budget is retried with half as many rows, and a single row that does fails.
pub(crate) fn backfill(
    engine: &mut Engine,
    id: &DeploymentId,
    mut batch: Backfill,
    cancellation: &Cancellation,
) -> crate::Result<(Backfill, Vec<Value>)> {
    let mut rows = batch.rows.len();
    loop {
        let started = Instant::now();
        let (mut outputs, mut bytes) = (Vec::with_capacity(rows), 0);
        let mut exceeded = None;
        for row in &batch.rows[..rows] {
            let to = (batch.migration.as_str(), batch.table.as_str(), Direction::To);
            let output = transform(engine, id, to, &row.input, cancellation)
                .map_err(|reason| Error::from(batch.failure(&row.id, &reason)))?;
            bytes += output.to_string().len();
            if started.elapsed() > BATCH_DEADLINE || bytes > BATCH_OUTPUT_BYTES {
                exceeded = Some(row.id.clone());
                break;
            }
            outputs.push(output);
        }
        match exceeded {
            None => {
                batch.shrink(rows);
                return Ok((batch, outputs));
            }
            Some(_) if rows > 1 => rows /= 2,
            Some(row) => {
                let reason =
                    format!("the transform exceeded the {BATCH_DEADLINE:?} or {BATCH_OUTPUT_BYTES} byte batch budget");
                return Err(batch.failure(&row, &reason).into());
            }
        }
    }
}
