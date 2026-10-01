//! Runs migration transforms in a deployment's bundle. A transform is a pure function of one row: it can't read,
//! `Date.now()` returns 0 and `Math.random()` throws, so it returns the same fields on every call.

use chunk_contract::Deployment;
use chunk_js::{Cancellation, DeploymentId, Engine, Invocation, Key, Limits, Mode, ReadHost};
use chunk_store::TransformError;
use serde_json::{Value, json};

/// Which transform of a migration runs.
#[derive(Clone, Copy)]
pub(crate) enum Direction {
    To,
    Back,
}

/// Maps `rows` through `migration`'s transform for `table` in the bundle of the resident deployment `id`.
pub(crate) fn transform(
    engine: &mut Engine,
    id: &DeploymentId,
    (migration, table, direction): (&str, &str, Direction),
    rows: &[Value],
) -> Result<Vec<Value>, TransformError> {
    let direction = match direction {
        Direction::To => "to",
        Direction::Back => "back",
    };
    let arguments = json!({"migration": migration, "table": table, "direction": direction, "rows": rows});
    let invocation = Invocation {
        export: "__chunk_migrate".into(),
        arguments: arguments.into(),
        caller: Value::Null.into(),
        mode: Mode::Query,
        timestamp: 0,
        seed: 0,
    };
    let execution =
        engine.execute(id, invocation, Box::new(NoReads), &Cancellation::default()).map_err(|error| match error {
            chunk_js::Error::Invalid(reason) if reason.ends_with("size limit") => TransformError::Limit,
            error => TransformError::Failed(error.to_string()),
        })?;
    serde_json::from_str(&execution.value).map_err(|error| TransformError::Failed(error.to_string()))
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
    pub fn to(
        &mut self,
        deployment: &Deployment,
        migration: &str,
        table: &str,
        rows: &[Value],
    ) -> Result<Vec<Value>, TransformError> {
        let failed = |error: &dyn std::fmt::Display| TransformError::Failed(error.to_string());
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
        transform(engine, &id, (migration, table, Direction::To), rows)
    }
}
