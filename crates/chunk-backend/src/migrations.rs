//! Runs migration transforms in a deployment's bundle, one engine invocation per row. Every invocation has the same
//! seed and `Date.now()` returns 0, and a transform can't read, so a row's result never depends on which rows
//! were transformed before it, nor on whether it was transformed in a backfill or by a write.

use chunk_contract::Deployment;
use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Key, Limits, Mode, ReadHost};
use chunk_store::MAX_DOCUMENT_BYTES;
use serde_json::{Value, json};

/// The most JSON one invocation takes or returns: a maximum-size document, plus room for its `_id` and the call's
/// envelope.
const INVOCATION_BYTES: usize = MAX_DOCUMENT_BYTES + 4096;

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
) -> Result<Value, String> {
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
        .execute_sized(id, invocation, Box::new(NoReads), &Cancellation::default(), INVOCATION_BYTES)
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

/// The commit thread's engine for backfills, holding one deployment's bundle at a time.
#[derive(Default)]
pub(crate) struct Migrator {
    engine: Option<Engine>,
    loaded: Option<DeploymentId>,
}

impl Migrator {
    pub fn to(&mut self, deployment: &Deployment, migration: &str, table: &str, row: &Value) -> Result<Value, String> {
        let failed = |error: &dyn std::fmt::Display| error.to_string();
        let id = DeploymentId::new(&deployment.id).map_err(|error| failed(&error))?;
        let engine = match &mut self.engine {
            Some(engine) => engine,
            empty => empty.insert(Engine::new().map_err(|error| failed(&error))?),
        };
        if self.loaded.as_ref() != Some(&id) {
            if let Some(loaded) = self.loaded.take() {
                engine.release(&loaded);
            }
            engine
                .register(id.clone(), deployment.source.clone(), Limits::default())
                .map_err(|error| failed(&error))?;
            self.loaded = Some(id.clone());
        }
        transform(engine, &id, (migration, table, Direction::To), row)
    }
}
