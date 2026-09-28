use std::{sync::Arc, time::Duration};

use chunk_contract::{Schema, validate_wire_value};
use chunk_proto::{
    sync::v1 as sync,
    v1::{Assignment, ClaimIdentity, PlayerDelivery},
};
use prost::Message;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{Control, Error, Generation, JvmIdentity, Release, Result, RuntimeConnection};

pub(crate) const MAX_JSON: usize = 48 * 1024;

/// How long a JVM may take to answer a cancelled method before its outcome counts as unknown.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// An authority-owned snapshot of one arrived player in one exact session generation.
#[derive(Clone)]
pub struct CapturedSession {
    claim: ClaimIdentity,
    delivery: PlayerDelivery,
    identity: JvmIdentity,
    session_type: String,
    /// The release the session's host runs, which declares its methods.
    release: Arc<Release>,
}

/// An immutable, server-minted operation. Retrying this value never allocates another operation.
#[derive(Clone)]
pub struct PreparedSessionMethod {
    target: CapturedSession,
    operation_id: String,
    /// The operation's place in its JVM's order of methods.
    sequence: u64,
    /// The call as its JVM's topic carries it, not yet cancelled.
    call: sync::JvmMethodCall,
    result: Schema,
}

impl PreparedSessionMethod {
    pub(crate) fn deadline_ms(&self) -> u64 {
        self.call.deadline_ms
    }
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }
}

/// How a session method call ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodOutcome {
    /// How the JVM says the method ended; `None` when control can't confirm whether it ran.
    pub phase: Option<sync::JvmMethodPhase>,
    /// What the method returned as JSON, set only when it completed.
    pub result_json: String,
}

impl MethodOutcome {
    const UNKNOWN: Self = Self { phase: None, result_json: String::new() };
}

impl Control {
    /// Captures only an exact, currently arrived ownership claim. Source references grant no authority.
    /// # Errors
    /// Rejects stale membership, departure and an unavailable process.
    pub fn capture_session(&self, identity: &ClaimIdentity) -> Result<CapturedSession> {
        let state = self.state()?;
        let claim = state.arrived_claim(identity).ok_or(Error::Invalid("stale method caller"))?;
        let session = state.sessions.get(&claim.session).ok_or(Error::Invalid("missing method session"))?;
        if session.retired {
            return Err(Error::Invalid("method session retired"));
        }
        let host = state.hosts.get(&session.host).ok_or(Error::Invalid("missing method host"))?;
        if host.retired {
            return Err(Error::Invalid("method host retired"));
        }
        let release = state.host_release(&session.host)?.clone();
        let runtime = self.host.connection(&session.host).ok_or(Error::Unresolved("method process unavailable"))?;
        let assignment = Assignment::decode(claim.assignment.as_deref().ok_or(Error::Invalid("missing assignment"))?)?;
        let delivery = assignment.delivery.ok_or(Error::Invalid("missing delivered identity"))?;
        if !crate::placement::runs_host(&state, &runtime, host)
            || runtime.identity.host != delivery.runtime_id
            || runtime.identity.generation != delivery.process_generation
            || release.session_types.get(&session.session_type).is_none_or(|spec| spec.app != runtime.identity.app)
        {
            return Err(Error::Invalid("method process changed"));
        }
        Ok(CapturedSession {
            claim: identity.clone(),
            delivery,
            identity: runtime.identity,
            session_type: session.session_type.clone(),
            release,
        })
    }

    /// Freezes a declared method, its arguments and deadline under a new operation identity.
    /// # Errors
    /// Rejects invalid schemas, oversized values, stale targets and timeouts outside 1..30000 ms.
    pub fn prepare_session_method(
        &self,
        target: &CapturedSession,
        name: &str,
        mut arguments: Value,
        timeout: Duration,
    ) -> Result<PreparedSessionMethod> {
        self.method_runtime(target)?;
        let method = target
            .release
            .contracts
            .session_methods
            .as_ref()
            .and_then(|methods| {
                methods.methods.iter().find(|method| {
                    method.app == target.identity.app
                        && format!("{}/{}", method.app, method.session) == target.session_type
                        && method.name == name
                })
            })
            .ok_or(Error::Invalid("undeclared session method"))?;
        method.arguments.normalize_api(&mut arguments);
        validate_wire_value(&arguments).map_err(Error::Invalid)?;
        let json = serde_json::to_string(&arguments)?;
        let timeout_ms: u64 = timeout.as_millis().try_into().map_err(|_| Error::Invalid("method timeout"))?;
        if !(1..=30_000).contains(&timeout_ms) || json.len() > MAX_JSON || !method.arguments.accepts(&arguments) {
            return Err(Error::Invalid("invalid session method arguments or timeout"));
        }
        // The allocating commit's generation increases across restores, as JVMs require of sequences.
        let mut writer = self.authority.writer()?;
        writer.update(|state| {
            state.method_sequence = Generation::PENDING.wire();
            Ok(())
        })?;
        let sequence = self.state()?.method_sequence;
        drop(writer);
        let call = sync::JvmMethodCall {
            session: target.delivery.session.as_ref().map(|session| session.id.clone()).unwrap_or_default(),
            method: name.into(),
            arguments_json: json.into_bytes(),
            delivery: target.delivery.operation_id.clone(),
            deadline_ms: crate::now_ms().checked_add(timeout_ms).ok_or(Error::Capacity)?,
            cancel: false,
        };
        Ok(PreparedSessionMethod {
            target: target.clone(),
            operation_id: format!("{}/{sequence}", target.identity.process_id),
            sequence,
            call,
            result: method.result.clone(),
        })
    }

    /// Puts the operation on its JVM's topic, unless it is already there, and waits for the JVM's result. A cancelled
    /// or expired call asks the JVM not to start it, and without an answer in time its outcome is unknown; the entry
    /// stays until the JVM answers, so a retry never runs the method again. A call already cancelled or expired goes
    /// on the topic cancelled. A retry after its result's retention ended, or once the JVM is gone, is unknown.
    /// # Errors
    /// Rejects malformed results, and a new method once its JVM's method budget is full.
    pub async fn call_session_method(
        &self,
        operation: &PreparedSessionMethod,
        cancellation: &CancellationToken,
    ) -> Result<MethodOutcome> {
        let host = &operation.target.identity.host;
        if self.host.connection(host).is_none_or(|runtime| runtime.identity != operation.target.identity) {
            return Ok(MethodOutcome::UNKNOWN);
        }
        let stopped = || {
            cancellation.is_cancelled()
                || crate::now_ms() >= operation.deadline_ms()
                || self.method_runtime(&operation.target).is_err()
        };
        let topic = sync::JvmMethodCall { cancel: stopped(), ..operation.call.clone() };
        let Some(mut call) = self.jvms.call(host, &operation.operation_id, operation.sequence, topic)? else {
            return Ok(MethodOutcome::UNKNOWN);
        };
        let mut cancelled = None;
        loop {
            if let Some(result) = call.result() {
                return outcome(operation, &result);
            }
            if cancelled.is_none() && stopped() {
                self.jvms.cancel(host, &operation.operation_id);
                cancelled = Some(tokio::time::Instant::now());
            }
            if cancelled.is_some_and(|at| at.elapsed() >= CANCEL_GRACE) {
                return Ok(MethodOutcome::UNKNOWN);
            }
            tokio::select! {
                changed = call.changed() => if !changed { return Ok(MethodOutcome::UNKNOWN) },
                () = tokio::time::sleep(Duration::from_millis(100)) => {},
                () = cancellation.cancelled(), if cancelled.is_none() => {},
            }
        }
    }

    /// Calls `operation` until it finishes, calling the same frozen operation again while its outcome is unknown,
    /// until its deadline passes or `stop` fires, which cancels it through `cancellation`.
    pub async fn run_session_method(
        &self,
        operation: &PreparedSessionMethod,
        cancellation: &CancellationToken,
        stop: &CancellationToken,
    ) -> MethodOutcome {
        loop {
            let response = self.call_session_method(operation, cancellation).await.unwrap_or(MethodOutcome::UNKNOWN);
            if response.phase.is_some() || crate::now_ms() >= operation.deadline_ms() || stop.is_cancelled() {
                return response;
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(100)) => {},
                () = stop.cancelled() => cancellation.cancel(),
            }
        }
    }

    pub(crate) fn method_runtime(&self, target: &CapturedSession) -> Result<RuntimeConnection> {
        let current = self.capture_session(&target.claim)?;
        if current.identity != target.identity
            || current.delivery != target.delivery
            || current.session_type != target.session_type
        {
            return Err(Error::Invalid("captured session changed"));
        }
        self.host.connection(&target.identity.host).ok_or(Error::Unresolved("method process unavailable"))
    }
}

/// The outcome of `result`, which a JVM sent for `operation`: a completed method's result must match its declaration.
fn outcome(operation: &PreparedSessionMethod, result: &sync::JvmMethodResult) -> Result<MethodOutcome> {
    let phase = result.phase();
    match phase {
        sync::JvmMethodPhase::Unspecified => Err(Error::Invalid("invalid session method phase")),
        sync::JvmMethodPhase::Completed => {
            let mut value: Value = serde_json::from_slice(&result.result_json)?;
            operation.result.normalize_api(&mut value);
            validate_wire_value(&value).map_err(Error::Invalid)?;
            if !operation.result.accepts(&value) {
                return Err(Error::Invalid("invalid session method result"));
            }
            Ok(MethodOutcome { phase: Some(phase), result_json: serde_json::to_string(&value)? })
        }
        sync::JvmMethodPhase::Cancelled | sync::JvmMethodPhase::Failed => {
            Ok(MethodOutcome { phase: Some(phase), result_json: String::new() })
        }
    }
}
