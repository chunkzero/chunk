use crate::{Cancellation, DeploymentId, Engine, Error, Execution, Invocation, Limits, ReadHost};

/// Convenience owner for a single deployment on the caller's thread.
/// Backends hosting multiple versions should share one `Engine` instead.
pub struct Deployment {
    id: DeploymentId,
    engine: Engine,
}

impl Deployment {
    /// # Errors
    /// Rejects invalid identity, source or limits, initialization failures and budgets.
    pub fn new(id: String, source: String, limits: Limits) -> Result<Self, Error> {
        let id = DeploymentId::new(id)?;
        let mut engine = Engine::new()?;
        engine.register(id.clone(), source, limits)?;
        Ok(Self { id, engine })
    }

    pub fn id(&self) -> &str {
        self.id.as_str()
    }

    /// Executes synchronously on the owning thread with fresh host capabilities.
    /// # Errors
    /// Reports invalid input, execution errors, cancellation and heap/time limits.
    pub fn execute(
        &mut self,
        invocation: Invocation,
        host: Box<dyn ReadHost>,
        cancellation: &Cancellation,
    ) -> Result<Execution, Error> {
        self.engine.execute(&self.id, invocation, host, cancellation)
    }
}
