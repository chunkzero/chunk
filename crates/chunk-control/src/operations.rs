use std::sync::{Arc, Mutex, PoisonError};

use tokio_util::task::TaskTracker;

use crate::{Error, Result};

/// Control's accepted operations, which run to completion even if their call is dropped, and which control awaits
/// before it stops hosts. Once shutdown begins, external operations are refused, so the operations it awaits are
/// finite.
#[derive(Clone, Default)]
pub struct Operations {
    pub(crate) tracker: TaskTracker,
    closed: Arc<Mutex<bool>>,
}

impl Operations {
    /// Runs a claim-lifecycle operation from a proxy or gateway.
    /// # Errors
    /// Refuses the operation with `control draining` once shutdown has begun, and reports its own errors or a failed
    /// task.
    pub async fn admit<T: Send + 'static>(
        &self,
        operation: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        // Closing takes the same lock, so no admitted operation is spawned after shutdown's wait began.
        let task = {
            let closed = self.closed.lock().unwrap_or_else(PoisonError::into_inner);
            if *closed {
                return Err(Error::Invalid("control draining"));
            }
            self.tracker.spawn(operation)
        };
        task.await.unwrap_or(Err(Error::Unresolved("the control operation's task failed")))
    }

    /// Runs a JVM's report, which shutdown still applies while it stops the JVM's host.
    /// # Errors
    /// Reports the report's own errors or a failed task.
    pub async fn report(&self, report: impl Future<Output = Result<()>> + Send + 'static) -> Result<()> {
        let task = self.tracker.spawn(report);
        task.await.unwrap_or(Err(Error::Unresolved("the control operation's task failed")))
    }

    /// Refuses further operations from proxies and gateways.
    pub(crate) fn close(&self) {
        *self.closed.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.tracker.close();
    }

    pub(crate) async fn wait(&self) {
        self.tracker.wait().await;
    }
}
