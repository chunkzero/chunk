//! Control's effects on a JVM registered over sync: it places, withdraws and calls through the JVM's topic, and learns
//! each outcome from the JVM's reports and method results.

use std::time::Duration;

use chunk_proto::{
    sync::v1 as sync,
    v1::{ConfigurationResponse, DeliveryPhase, DeploymentRef, PlayerPreparation, ProcessIdentity},
};
use tokio::{sync::watch, time::Instant};

use super::{Jvms, Method, Work};
use crate::{Control, Error, Generation, Result, RuntimeConnection, state::Phase};

/// How long control waits for a JVM to report a delivery prepared or closed.
const DELIVERY: Duration = Duration::from_secs(10);

/// How long a JVM keeps a method's result for a retry of its call.
const RESULT_RETENTION: Duration = Duration::from_secs(300);

/// A session method on a JVM's topic, awaiting the JVM's result.
pub(crate) struct MethodCall {
    work: watch::Receiver<Work>,
    operation: String,
}

impl MethodCall {
    /// The result the JVM sent, once it has.
    pub fn result(&self) -> Option<sync::JvmMethodResult> {
        let work = self.work.borrow();
        work.methods.get(&self.operation)?.result.as_ref().map(|(_, result)| result.clone())
    }

    /// Waits for the JVM's work to change. `false` once the JVM is gone.
    pub async fn changed(&mut self) -> bool {
        self.work.changed().await.is_ok()
    }
}

impl Jvms {
    /// Puts session method `operation` on `host`'s topic unless it is already there, as for a retry. `None` for a host
    /// whose JVM has not registered over sync.
    pub fn call(&self, host: &str, operation: &str, call: sync::JvmMethodCall) -> Option<MethodCall> {
        let jvms = self.lock().ok()?;
        let jvm = jvms.get(host)?;
        jvm.work.send_if_modified(|work| {
            if work.methods.contains_key(operation) {
                return false;
            }
            let retained =
                |method: &Method| method.result.as_ref().is_none_or(|(at, _)| at.elapsed() < RESULT_RETENTION);
            work.methods.retain(|_, method| retained(method));
            work.methods.insert(operation.into(), Method { call, result: None });
            true
        });
        Some(MethodCall { work: jvm.work.subscribe(), operation: operation.into() })
    }

    /// Asks `host`'s JVM not to start session method `operation`.
    pub fn cancel(&self, host: &str, operation: &str) {
        if let Ok(jvms) = self.lock()
            && let Some(jvm) = jvms.get(host)
        {
            jvm.work.send_if_modified(|work| match work.methods.get_mut(operation) {
                Some(method) if method.result.is_none() && !method.call.cancel => {
                    method.call.cancel = true;
                    true
                }
                _ => false,
            });
        }
    }
}

impl Control {
    /// Records how session method `operation` ended, as the JVM running `host` sent it on its topic stream `stream`.
    /// The first result stands, and one for a method control no longer asks for changes nothing.
    /// # Errors
    /// Reports a superseded stream as stopped, and rejects a malformed result.
    pub fn method_result(
        &self,
        host: &str,
        stream: &str,
        operation: &str,
        result: sync::JvmMethodResult,
    ) -> Result<()> {
        let completed = result.phase() == sync::JvmMethodPhase::Completed;
        if result.phase() == sync::JvmMethodPhase::Unspecified
            || (!completed && !result.result_json.is_empty())
            || result.result_json.len() > crate::session_methods::MAX_JSON
        {
            return Err(Error::Invalid("invalid session method result"));
        }
        let jvms = self.jvms.lock()?;
        let jvm = jvms.get(host).ok_or(Error::Stopped)?;
        if jvm.stream.as_ref().is_none_or(|current| current.id != stream) {
            return Err(Error::Stopped);
        }
        jvm.work.send_if_modified(|work| match work.methods.get_mut(operation) {
            Some(method) if method.result.is_none() => {
                method.result = Some((Instant::now(), result));
                true
            }
            _ => false,
        });
        Ok(())
    }

    /// The destination metadata of `runtime`, a JVM registered over sync, which its registration stated.
    pub(crate) fn jvm_configuration(
        &self,
        deployment: &DeploymentRef,
        runtime: &RuntimeConnection,
    ) -> Result<ConfigurationResponse> {
        let host = &runtime.identity.runtime_id;
        let protocol = self.jvms.protocol(host).ok_or(Error::Unresolved("the JVM has not registered over sync"))?;
        Ok(ConfigurationResponse {
            deployment: Some(deployment.clone()),
            process_generation: runtime.identity.generation,
            protocol,
            runtime_id: host.clone(),
        })
    }

    /// Waits for `runtime`, a JVM registered over sync, to report the delivery of claim `operation`, created at
    /// `generation`, prepared, which its topic asks for.
    /// # Errors
    /// Reports a delivery the JVM closed, or did not prepare within 10 seconds.
    pub(crate) async fn prepared_over_sync(
        &self,
        runtime: &RuntimeConnection,
        operation: &str,
        generation: Generation,
    ) -> Result<PlayerPreparation> {
        let host = &runtime.identity.runtime_id;
        let capability = self
            .reported(host, &runtime.identity, operation, generation, |phase| match phase {
                Some(DeliveryPhase::Closed) => Some(Err(Error::Unresolved("the JVM closed the delivery"))),
                _ => self.jvms.capability(host, operation).map(Ok),
            })
            .await?;
        Ok(PlayerPreparation { operation_id: operation.into(), endpoint: runtime.player_endpoint.clone(), capability })
    }

    /// Waits for `identity`, the JVM running `host` over sync, to close the delivery of withdrawing claim `operation`,
    /// created at `generation`, or for its claim to be released.
    /// # Errors
    /// Reports a delivery the JVM did not close within 10 seconds.
    pub(crate) async fn withdrawn_over_sync(
        &self,
        host: &str,
        identity: &ProcessIdentity,
        operation: &str,
        generation: Generation,
    ) -> Result<()> {
        self.reported(host, identity, operation, generation, |phase| {
            let released = self
                .state()
                .map(|state| state.claims.get(operation).is_none_or(|claim| claim.phase == Phase::Released));
            match released {
                Ok(released) if released || phase == Some(DeliveryPhase::Closed) => Some(Ok(())),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .await
    }

    /// Checks the phase `host`'s JVM last reported for `operation`'s delivery at `generation` after each report, until
    /// `check` returns an outcome.
    async fn reported<T>(
        &self,
        host: &str,
        identity: &ProcessIdentity,
        operation: &str,
        generation: Generation,
        check: impl Fn(Option<DeliveryPhase>) -> Option<Result<T>>,
    ) -> Result<T> {
        let mut reports = self.links.subscribe();
        let reported = async {
            loop {
                let binding = self.links.delivery(host, identity, operation);
                let binding = binding.filter(|binding| {
                    binding.delivery.as_ref().is_some_and(|delivery| delivery.owner_generation == generation.wire())
                });
                if let Some(outcome) = check(binding.and_then(|binding| DeliveryPhase::try_from(binding.phase).ok())) {
                    return outcome;
                }
                reports.changed().await.map_err(|_| Error::Unresolved("control stopped"))?;
            }
        };
        tokio::time::timeout(DELIVERY, reported)
            .await
            .unwrap_or(Err(Error::Unresolved("the JVM did not report the delivery in time")))
    }
}
