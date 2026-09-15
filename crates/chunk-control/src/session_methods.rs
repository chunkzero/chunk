use std::time::Duration;

use chunk_contract::{Schema, validate_wire_value};
use chunk_proto::v1::{
    Assignment, ClaimIdentity, PlayerDelivery, ProcessIdentity, SessionMethodCaller, SessionMethodPhase,
    SessionMethodRequest, SessionMethodResult, session_methods_client::SessionMethodsClient,
};
use prost::Message;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    Control, Error, Result, RuntimeConnection,
    placement::{auth, channel},
    state::Phase,
};

const MAX_JSON: usize = 48 * 1024;

/// An authority-owned snapshot of one arrived player in one exact session generation.
#[derive(Clone)]
pub struct CapturedSession {
    claim: ClaimIdentity,
    delivery: PlayerDelivery,
    identity: ProcessIdentity,
    session_type: String,
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
        let claim = state.claims.get(&identity.operation_id).ok_or(Error::Invalid("unknown method caller"))?;
        if claim.identity(&identity.operation_id) != *identity
            || claim.phase != Phase::Arrived
            || state.players.get(&claim.player).and_then(|owner| owner.current.as_ref()) != Some(&identity.operation_id)
        {
            return Err(Error::Invalid("stale method caller"));
        }
        let session = state.sessions.get(&claim.session).ok_or(Error::Invalid("missing method session"))?;
        if session.retired {
            return Err(Error::Invalid("method session retired"));
        }
        let host = state.hosts.get(&session.host).ok_or(Error::Invalid("missing method host"))?;
        if host.retired {
            return Err(Error::Invalid("method host retired"));
        }
        let runtime = self.host.connection(&session.host).ok_or(Error::Unresolved("method process unavailable"))?;
        let assignment = Assignment::decode(claim.assignment.as_deref().ok_or(Error::Invalid("missing assignment"))?)?;
        let delivery = assignment.delivery.ok_or(Error::Invalid("missing delivered identity"))?;
        if runtime.identity.deployment.as_ref() != Some(&self.config.deployment)
            || runtime.identity.runtime_id != delivery.runtime_id
            || runtime.identity.generation != delivery.process_generation
            || runtime.identity.machine_profile != host.profile
            || runtime.identity.app_id != host.app
            || self.config.apps.get(&host.app).is_none_or(|app| app.sha256 != runtime.identity.artifact_digest)
            || self
                .config
                .session_types
                .get(&session.session_type)
                .is_none_or(|spec| spec.app != runtime.identity.app_id)
        {
            return Err(Error::Invalid("method process changed"));
        }
        Ok(CapturedSession {
            claim: identity.clone(),
            delivery,
            identity: runtime.identity,
            session_type: session.session_type.clone(),
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
        let method = self
            .config
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
        let sequence = self.update(|state| {
            let sequence = state
                .method_sequence
                .checked_add(1)
                .filter(|value| i64::try_from(*value).is_ok())
                .ok_or(Error::Capacity)?;
            state.method_sequence = sequence;
            Ok(sequence)
        })?;
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

    /// Calls or polls the same operation. Cancellation can only prove non-execution while still queued.
    /// # Errors
    /// Rejects malformed authenticated replies. An unavailable reply returns UNKNOWN, retaining the same operation ID.
    pub async fn call_session_method(
        &self,
        operation: &PreparedSessionMethod,
        cancellation: &CancellationToken,
    ) -> Result<SessionMethodResult> {
        let runtime = self.host.connection(&operation.target.identity.runtime_id);
        let Some(runtime) = runtime.filter(|runtime| runtime.identity == operation.target.identity) else {
            return Ok(unknown(operation));
        };
        let Ok(connection) = channel(&runtime).await else {
            return Ok(unknown(operation));
        };
        let mut client = SessionMethodsClient::new(connection)
            .max_decoding_message_size(MAX_JSON + 8192)
            .max_encoding_message_size(MAX_JSON + 8192);
        loop {
            if cancellation.is_cancelled()
                || crate::now_ms() >= operation.request.deadline_ms
                || self.method_runtime(&operation.target).is_err()
            {
                let result = client.cancel(auth(&runtime, operation.request.clone(), 2)?).await;
                return match result {
                    Ok(reply) => validate_result(operation, reply.into_inner()),
                    Err(_) => Ok(unknown(operation)),
                };
            }
            let request = client.call(auth(&runtime, operation.request.clone(), 2)?);
            let response = tokio::select! {
                response = request => response,
                () = cancellation.cancelled() => continue,
            };
            let result = match response {
                Ok(reply) => validate_result(operation, reply.into_inner())?,
                Err(_) => return Ok(unknown(operation)),
            };
            if result.phase != SessionMethodPhase::Accepted as i32 {
                return Ok(result);
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_millis(10)) => {},
                () = cancellation.cancelled() => {},
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

fn unknown(operation: &PreparedSessionMethod) -> SessionMethodResult {
    SessionMethodResult {
        operation_id: operation.request.operation_id.clone(),
        phase: SessionMethodPhase::Unknown as i32,
        ..Default::default()
    }
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
