use std::{sync::Arc, time::Duration};

use chunk_contract::{Schema, validate_wire_value};
use chunk_proto::{
    sync::v1 as sync,
    v1::{
        Assignment, ClaimIdentity, PlayerDelivery, ProcessIdentity, SessionMethodCaller, SessionMethodPhase,
        SessionMethodRequest, SessionMethodResult,
    },
};
use prost::Message;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{Control, Error, Generation, Release, Result, RuntimeConnection};

pub(crate) const MAX_JSON: usize = 48 * 1024;

/// How long a JVM may take to answer a cancelled method before its outcome counts as unknown.
const CANCEL_GRACE: Duration = Duration::from_secs(2);

/// An authority-owned snapshot of one arrived player in one exact session generation.
#[derive(Clone)]
pub struct CapturedSession {
    claim: ClaimIdentity,
    delivery: PlayerDelivery,
    identity: ProcessIdentity,
    session_type: String,
    /// The release the session's host runs, which declares its methods.
    release: Arc<Release>,
}

/// An immutable, server-minted operation. Retrying this value never allocates another operation.
#[derive(Clone)]
pub struct PreparedSessionMethod {
    target: CapturedSession,
    request: SessionMethodRequest,
    result: Schema,
}

impl CapturedSession {
    pub(crate) fn matches_declaration(&self, app: &str, session: &str) -> bool {
        self.identity.app_id == app && self.session_type == format!("{app}/{session}")
    }
}

impl PreparedSessionMethod {
    pub(crate) fn target(&self) -> &CapturedSession {
        &self.target
    }
    pub(crate) fn deadline_ms(&self) -> u64 {
        self.request.deadline_ms
    }
    pub(crate) fn retained_bytes(&self) -> Result<usize> {
        Ok(self.request.encoded_len()
            + self.target.delivery.encoded_len()
            + self.target.claim.encoded_len()
            + self.target.identity.encoded_len()
            + self.target.session_type.len()
            + serde_json::to_vec(&self.result)?.len())
    }
    #[must_use]
    pub fn operation_id(&self) -> &str {
        &self.request.operation_id
    }
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
            || runtime.identity.runtime_id != delivery.runtime_id
            || runtime.identity.generation != delivery.process_generation
            || release.session_types.get(&session.session_type).is_none_or(|spec| spec.app != runtime.identity.app_id)
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
                    method.app == target.identity.app_id
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
        let issued_at_ms = crate::now_ms();
        let request = SessionMethodRequest {
            identity: Some(target.identity.clone()),
            operation_id: format!("{}/{sequence}", target.identity.process_id),
            sequence,
            session: target.delivery.session.clone(),
            session_generation: target.delivery.session_generation,
            session_type: target.session_type.clone(),
            method: name.into(),
            arguments_json: json,
            caller: Some(SessionMethodCaller {
                delivery_operation_id: target.delivery.operation_id.clone(),
                player: target.delivery.player.clone(),
                membership_generation: target.delivery.membership_generation,
                owner_generation: target.delivery.owner_generation,
            }),
            issued_at_ms,
            deadline_ms: issued_at_ms.checked_add(timeout_ms).ok_or(Error::Capacity)?,
        };
        Ok(PreparedSessionMethod { target: target.clone(), request, result: method.result.clone() })
    }

    /// Checks that the operation's JVM, if it registered, has room for another method.
    /// # Errors
    /// Reports a full method budget as over capacity.
    pub(crate) fn admits_method(&self, operation: &PreparedSessionMethod) -> Result<()> {
        self.jvms.admits(&operation.target.identity.runtime_id, &sync_call(operation, false))
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
    ) -> Result<SessionMethodResult> {
        let request = &operation.request;
        let host = &operation.target.identity.runtime_id;
        if self.host.connection(host).is_none_or(|runtime| runtime.identity != operation.target.identity) {
            return Ok(unknown(operation));
        }
        let stopped = || {
            cancellation.is_cancelled()
                || crate::now_ms() >= request.deadline_ms
                || self.method_runtime(&operation.target).is_err()
        };
        let call = self.jvms.call(host, &request.operation_id, request.sequence, sync_call(operation, stopped()))?;
        let Some(mut call) = call else {
            return Ok(unknown(operation));
        };
        let mut cancelled = None;
        loop {
            if let Some(result) = call.result() {
                return validate_result(operation, from_sync(&request.operation_id, result)?);
            }
            if cancelled.is_none() && stopped() {
                self.jvms.cancel(host, &request.operation_id);
                cancelled = Some(tokio::time::Instant::now());
            }
            if cancelled.is_some_and(|at| at.elapsed() >= CANCEL_GRACE) {
                return Ok(unknown(operation));
            }
            tokio::select! {
                changed = call.changed() => if !changed { return Ok(unknown(operation)) },
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
    ) -> SessionMethodResult {
        loop {
            let response =
                self.call_session_method(operation, cancellation).await.unwrap_or_else(|_| unknown(operation));
            if response.phase != SessionMethodPhase::Unknown as i32
                || crate::now_ms() >= operation.deadline_ms()
                || stop.is_cancelled()
            {
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
        self.host.connection(&target.identity.runtime_id).ok_or(Error::Unresolved("method process unavailable"))
    }
}

/// The operation as its JVM's topic carries it.
fn sync_call(operation: &PreparedSessionMethod, cancel: bool) -> sync::JvmMethodCall {
    let request = &operation.request;
    sync::JvmMethodCall {
        session: request.session.as_ref().map(|session| session.id.clone()).unwrap_or_default(),
        method: request.method.clone(),
        arguments_json: request.arguments_json.clone().into_bytes(),
        delivery: operation.target.delivery.operation_id.clone(),
        deadline_ms: request.deadline_ms,
        cancel,
    }
}

fn unknown(operation: &PreparedSessionMethod) -> SessionMethodResult {
    SessionMethodResult {
        operation_id: operation.request.operation_id.clone(),
        phase: SessionMethodPhase::Unknown as i32,
        ..Default::default()
    }
}

/// The result a JVM sent for `operation`, as control states it.
fn from_sync(operation: &str, result: sync::JvmMethodResult) -> Result<SessionMethodResult> {
    let phase = match result.phase() {
        sync::JvmMethodPhase::Completed => SessionMethodPhase::Completed,
        sync::JvmMethodPhase::Cancelled => SessionMethodPhase::Cancelled,
        sync::JvmMethodPhase::Failed => SessionMethodPhase::Failed,
        sync::JvmMethodPhase::Unspecified => return Err(Error::Invalid("invalid session method phase")),
    };
    Ok(SessionMethodResult {
        operation_id: operation.into(),
        phase: phase as i32,
        result_json: String::from_utf8(result.result_json)
            .map_err(|_| Error::Invalid("invalid session method result"))?,
        error: None,
    })
}

fn validate_result(operation: &PreparedSessionMethod, mut result: SessionMethodResult) -> Result<SessionMethodResult> {
    if result.operation_id != operation.request.operation_id || result.encoded_len() > MAX_JSON + 4096 {
        return Err(Error::Invalid("invalid session method reply"));
    }
    match SessionMethodPhase::try_from(result.phase) {
        Ok(SessionMethodPhase::Completed) => {
            let mut value: Value = serde_json::from_str(&result.result_json)?;
            operation.result.normalize_api(&mut value);
            validate_wire_value(&value).map_err(Error::Invalid)?;
            if !operation.result.accepts(&value) || result.error.is_some() {
                return Err(Error::Invalid("invalid session method result"));
            }
            result.result_json = serde_json::to_string(&value)?;
        }
        Ok(
            SessionMethodPhase::Accepted
            | SessionMethodPhase::Cancelled
            | SessionMethodPhase::Failed
            | SessionMethodPhase::Unknown,
        ) if result.result_json.is_empty() => {}
        _ => return Err(Error::Invalid("invalid session method phase")),
    }
    Ok(result)
}
