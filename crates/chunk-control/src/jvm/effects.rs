//! Control's effects on a JVM: it places, withdraws and calls through the JVM's topic, and learns each outcome from
//! the JVM's reports and method results.

use std::time::Duration;

use chunk_proto::{
    control::v1::{ConfigurationResponse, DeploymentRef, PlayerPreparation},
    sync::v1::{self as sync, JvmDeliveryPhase},
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

/// How many retired methods control remembers individually for one JVM.
const MAX_RETIRED: usize = 65_536;

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
    /// Drops the results kept past their retention, retiring their methods.
    pub(super) fn prune(&mut self) {
        let mut expired = Vec::new();
        self.methods.retain(|_, method| {
            let kept = method.result.as_ref().is_none_or(|(at, _)| at.elapsed() < RESULT_RETENTION);
            if !kept {
                expired.push(method.sequence);
            }
            kept
        });
        for sequence in expired {
            self.retire(sequence);
        }
    }

    /// Records the method numbered `sequence` as retired. Past the cap, the lowest retired sequence raises
    /// `retired_below` instead.
    fn retire(&mut self, sequence: u64) {
        self.retired.insert(sequence);
        while self.retired.len() > MAX_RETIRED {
            if let Some(lowest) = self.retired.pop_first() {
                self.retired_below = self.retired_below.max(lowest);
            }
        }
    }

    /// Whether the method numbered `sequence`, which `methods` does not hold, was retired.
    fn retired(&self, sequence: u64) -> bool {
        sequence <= self.retired_below || self.retired.contains(&sequence)
    }

    /// Forgets every method, retired ones too, as for a JVM its host confirmed stopped. Whether any was held.
    pub(super) fn forget_methods(&mut self) -> bool {
        self.retired.clear();
        self.retired_below = 0;
        !std::mem::take(&mut self.methods).is_empty()
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
    /// Puts session method `operation`, numbered `sequence`, on `host`'s topic unless it is already there, as for a
    /// retry. `None` for a host whose JVM has not registered, and for a method retired once its result's
    /// retention ended: its outcome is unknown, and the JVM is never asked about it again. The method counts against
    /// the JVM's method budget until the JVM answers it and its result's retention ends, or the JVM stops.
    /// # Errors
    /// Reports a full budget as over capacity.
    pub fn call(
        &self,
        host: &str,
        operation: &str,
        sequence: u64,
        call: sync::JvmMethodCall,
    ) -> Result<Option<MethodCall>> {
        let jvms = self.lock()?;
        let Some(jvm) = jvms.get(host) else {
            return Ok(None);
        };
        let mut held = Ok(true);
        jvm.work.send_if_modified(|work| {
            work.prune();
            if work.methods.contains_key(operation) {
                return false;
            }
            if work.retired(sequence) {
                held = Ok(false);
                return false;
            }
            let method = Method { sequence, call, result: None };
            held = work.admit(method.bytes()).map(|()| true);
            held.is_ok() && work.methods.insert(operation.into(), method).is_none()
        });
        Ok(held?.then(|| MethodCall { work: jvm.work.subscribe(), operation: operation.into() }))
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

    /// The destination metadata of `runtime`, which its registration stated.
    pub(crate) fn jvm_configuration(
        &self,
        deployment: &DeploymentRef,
        runtime: &RuntimeConnection,
    ) -> Result<ConfigurationResponse> {
        let host = &runtime.identity.host;
        let protocol = self.jvms.protocol(host).ok_or(Error::Unresolved("the JVM has not registered"))?;
        Ok(ConfigurationResponse {
            deployment: Some(deployment.clone()),
            process_generation: runtime.identity.generation,
            protocol,
            runtime_id: host.clone(),
        })
    }

    /// Waits for `runtime`'s JVM to report the delivery of claim `operation`, created at `generation`, prepared, which
    /// its topic asks for.
    /// # Errors
    /// Reports a delivery the JVM closed, whose claim was released, or that it did not prepare within 10 seconds.
    pub(crate) async fn prepared(
        &self,
        runtime: &RuntimeConnection,
        operation: &str,
        generation: Generation,
    ) -> Result<PlayerPreparation> {
        let host = &runtime.identity.host;
        let capability = self
            .reported(host, operation, generation, |reported, released| match reported {
                _ if released => Some(Err(Error::Invalid("claim no longer reserved"))),
                Some((JvmDeliveryPhase::Prepared, capability)) => Some(Ok(capability)),
                Some((JvmDeliveryPhase::Closed, _)) => Some(Err(Error::Unresolved("the JVM closed the delivery"))),
                _ => None,
            })
            .await?;
        Ok(PlayerPreparation { operation_id: operation.into(), endpoint: runtime.player_endpoint.clone(), capability })
    }

    /// Waits for the JVM running `host` to close the delivery of withdrawing claim `operation`, created at
    /// `generation`, or for its claim to be released.
    /// # Errors
    /// Reports a delivery the JVM did not close within 10 seconds.
    pub(crate) async fn withdrawn(&self, host: &str, operation: &str, generation: Generation) -> Result<()> {
        self.reported(host, operation, generation, |reported, released| {
            (released || reported.is_some_and(|(phase, _)| phase == JvmDeliveryPhase::Closed)).then_some(Ok(()))
        })
        .await
    }

    /// Checks the phase and capability `host`'s JVM last reported for `operation`'s delivery at `generation`, and
    /// whether its claim is released, after each report, until `check` returns an outcome.
    async fn reported<T>(
        &self,
        host: &str,
        operation: &str,
        generation: Generation,
        check: impl Fn(Option<(JvmDeliveryPhase, Vec<u8>)>, bool) -> Option<Result<T>>,
    ) -> Result<T> {
        let mut reports = self.links.subscribe();
        let reported = async {
            loop {
                let released = self.state()?.claims.get(operation).is_none_or(|claim| claim.phase == Phase::Released);
                if let Some(outcome) = check(self.jvms.delivery(host, operation, generation), released) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Jvms with one JVM, on `host`.
    fn jvms() -> Jvms {
        let jvm = super::super::Jvm {
            registration: sync::JvmRegistration::default(),
            stream: None,
            health: None,
            work: watch::Sender::default(),
            deliveries: std::collections::BTreeMap::new(),
        };
        let jvms = Jvms::default();
        jvms.lock().unwrap().insert("host".into(), jvm);
        jvms
    }

    fn call(jvms: &Jvms, sequence: u64) -> Option<MethodCall> {
        let call = sync::JvmMethodCall { method: "score".into(), ..sync::JvmMethodCall::default() };
        jvms.call("host", &format!("jvm/{sequence}"), sequence, call).unwrap()
    }

    /// Records the JVM completing method `sequence` at `at`.
    fn complete(jvms: &Jvms, sequence: u64, at: Instant) {
        let completed =
            sync::JvmMethodResult { phase: sync::JvmMethodPhase::Completed.into(), result_json: b"7".into() };
        jvms.lock().unwrap()["host"].work.send_modify(|work| {
            work.methods.get_mut(&format!("jvm/{sequence}")).unwrap().result = Some((at, completed));
        });
    }

    #[test]
    fn a_method_whose_result_expired_is_unknown_and_never_asked_again() {
        let jvms = jvms();
        let work = jvms.lock().unwrap()["host"].work.subscribe();
        assert!(call(&jvms, 5).is_some());
        complete(&jvms, 5, Instant::now().checked_sub(RESULT_RETENTION).unwrap());
        assert!(call(&jvms, 5).is_none());
        assert!(work.borrow().methods.is_empty());
    }

    #[test]
    fn a_method_first_called_after_a_later_one_retired_still_runs() {
        let jvms = jvms();
        assert!(call(&jvms, 2).is_some());
        complete(&jvms, 2, Instant::now().checked_sub(RESULT_RETENTION).unwrap());
        let first = call(&jvms, 1).expect("a first call runs");
        complete(&jvms, 1, Instant::now());
        assert_eq!(first.result().unwrap().phase(), sync::JvmMethodPhase::Completed);
        assert!(call(&jvms, 2).is_none());
    }

    #[test]
    fn retired_methods_past_the_cap_are_retired_by_watermark() {
        let mut work = Work::default();
        for sequence in (1..=MAX_RETIRED as u64 + 1).map(|sequence| sequence * 2) {
            work.retire(sequence);
        }
        assert_eq!((work.retired.len(), work.retired_below), (MAX_RETIRED, 2));
        assert!(work.retired(1) && work.retired(2) && !work.retired(3) && work.retired(4));
    }
}
