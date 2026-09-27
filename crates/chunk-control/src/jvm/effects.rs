//! Control's effects on a JVM registered over sync: it places, withdraws and calls through the JVM's topic, and learns
//! each outcome from the JVM's reports and method results.

use std::time::Duration;

use chunk_proto::{
    sync::v1 as sync,
    v1::{ConfigurationResponse, DeliveryPhase, DeploymentRef, PlayerPreparation},
};
use prost::Message;
use tokio::{sync::watch, time::Instant};

use super::{Jvms, Method, Work};
use crate::{Control, Error, Generation, Result, RuntimeConnection, session_methods::MAX_JSON, state::Phase};

/// How long control waits for a JVM to report a delivery prepared or closed.
const DELIVERY: Duration = Duration::from_secs(10);

/// How long control keeps a method's result for a retry of its call.
const RESULT_RETENTION: Duration = Duration::from_secs(300);

/// How many session methods control holds for one JVM, pending or answered.
const MAX_METHODS: usize = 256;

/// How many bytes of session methods control holds for one JVM.
const MAX_METHOD_BYTES: usize = 8 * 1024 * 1024;

/// The room a pending method holds for its result.
const RESULT_RESERVE: usize = MAX_JSON + 1024;

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

impl Work {
    /// Drops the results kept past their retention.
    pub(super) fn prune(&mut self) {
        self.methods.retain(|_, method| method.result.as_ref().is_none_or(|(at, _)| at.elapsed() < RESULT_RETENTION));
    }

    /// Checks that a method holding `bytes` fits the JVM's method budget.
    fn admit(&mut self, bytes: usize) -> Result<()> {
        self.prune();
        let held: usize = self.methods.values().map(Method::bytes).sum();
        if self.methods.len() >= MAX_METHODS || held + bytes > MAX_METHOD_BYTES {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}

impl Method {
    /// The bytes the method holds: its call, and its result or room for one.
    fn bytes(&self) -> usize {
        self.call.encoded_len() + self.result.as_ref().map_or(RESULT_RESERVE, |(_, result)| result.encoded_len())
    }
}

impl Jvms {
    /// Checks that `host`'s JVM, if it registered over sync, has room in its method budget for `call`.
    /// # Errors
    /// Reports a full budget as over capacity.
    pub fn admits(&self, host: &str, call: &sync::JvmMethodCall) -> Result<()> {
        let jvms = self.lock()?;
        let Some(jvm) = jvms.get(host) else {
            return Ok(());
        };
        let mut admitted = Ok(());
        jvm.work.send_if_modified(|work| {
            admitted = work.admit(call.encoded_len() + RESULT_RESERVE);
            false
        });
        admitted
    }

    /// Puts session method `operation` on `host`'s topic unless it is already there, as for a retry. `None` for a host
    /// whose JVM has not registered over sync. The method counts against the JVM's method budget until the JVM answers
    /// it and its result's retention ends, or the JVM stops.
    /// # Errors
    /// Reports a full budget as over capacity.
    pub fn call(&self, host: &str, operation: &str, call: sync::JvmMethodCall) -> Result<Option<MethodCall>> {
        let jvms = self.lock()?;
        let Some(jvm) = jvms.get(host) else {
            return Ok(None);
        };
        let mut admitted = Ok(());
        jvm.work.send_if_modified(|work| {
            if work.methods.contains_key(operation) {
                return false;
            }
            let method = Method { call, result: None };
            admitted = work.admit(method.bytes());
            admitted.is_ok() && work.methods.insert(operation.into(), method).is_none()
        });
        admitted?;
        Ok(Some(MethodCall { work: jvm.work.subscribe(), operation: operation.into() }))
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
            || result.result_json.len() > MAX_JSON
        {
            return Err(Error::Invalid("invalid session method result"));
        }
        let jvms = self.jvms.lock()?;
        let jvm = jvms.get(host).ok_or(Error::Stopped)?;
        if jvm.stream.as_ref().is_none_or(|current| current.id != stream) {
            return Err(Error::Stopped);
        }
        jvm.work.send_if_modified(|work| {
            work.prune();
            match work.methods.get_mut(operation) {
                Some(method) if method.result.is_none() => {
                    method.call.arguments_json = Vec::new();
                    method.result = Some((Instant::now(), result));
                    true
                }
                _ => false,
            }
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
            .reported(host, operation, generation, |reported| match reported {
                Some((DeliveryPhase::Prepared, capability)) => Some(Ok(capability)),
                Some((DeliveryPhase::Closed, _)) => Some(Err(Error::Unresolved("the JVM closed the delivery"))),
                _ => None,
            })
            .await?;
        Ok(PlayerPreparation { operation_id: operation.into(), endpoint: runtime.player_endpoint.clone(), capability })
    }

    /// Waits for the JVM running `host` over sync to close the delivery of withdrawing claim `operation`, created at
    /// `generation`, or for its claim to be released.
    /// # Errors
    /// Reports a delivery the JVM did not close within 10 seconds.
    pub(crate) async fn withdrawn_over_sync(&self, host: &str, operation: &str, generation: Generation) -> Result<()> {
        self.reported(host, operation, generation, |reported| {
            let released = self
                .state()
                .map(|state| state.claims.get(operation).is_none_or(|claim| claim.phase == Phase::Released));
            match released {
                Ok(released) if released || reported.is_some_and(|(phase, _)| phase == DeliveryPhase::Closed) => {
                    Some(Ok(()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            }
        })
        .await
    }

    /// Checks the phase and capability `host`'s JVM last reported for `operation`'s delivery at `generation` after each
    /// report, until `check` returns an outcome.
    async fn reported<T>(
        &self,
        host: &str,
        operation: &str,
        generation: Generation,
        check: impl Fn(Option<(DeliveryPhase, Vec<u8>)>) -> Option<Result<T>>,
    ) -> Result<T> {
        let mut reports = self.links.subscribe();
        let reported = async {
            loop {
                if let Some(outcome) = check(self.jvms.delivery(host, operation, generation)) {
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
