use super::super::super::{
    platform::{Platform, request},
    transport::invalid_data,
};
use super::{Tasks, effects::Method, scope::Origin};
use chunk_proto::v1::{PrepareSessionMethodRequest, PreparedMethodRequest, SessionMethodPhase, SessionMethodResult};
use serde_json::Value;
use std::{io, time::Duration};

struct Prepared {
    platform: Platform,
    operation: String,
    armed: bool,
    deadline: tokio::time::Instant,
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let platform = self.platform.clone();
        let operation_id = self.operation.clone();
        self.platform.cleanup.spawn(async move {
            if let Ok(request) = request(PreparedMethodRequest { operation_id }, &platform.target.control.token) {
                let _ = platform.control.clone().cancel_prepared_method(request).await;
            }
        });
    }
}

pub(super) async fn invoke(
    tasks: &Tasks,
    origin: &Origin,
    method: Method,
    arguments: Value,
    send: bool,
) -> io::Result<Value> {
    tokio::select! {
        biased;
        () = tasks.invocation.cancelled() => Err(invalid_data("command invocation canceled")),
        () = tasks.connection.cancelled() => Err(invalid_data("command connection canceled")),
        () = origin.cancellation.cancelled() => Err(invalid_data("captured session canceled")),
        result = tokio::time::timeout(Duration::from_secs(6), invoke_inner(tasks, origin, method, arguments, send)) => result.map_err(io::Error::other)?,
    }
}

async fn invoke_inner(
    tasks: &Tasks,
    origin: &Origin,
    method: Method,
    arguments: Value,
    send: bool,
) -> io::Result<Value> {
    if method.app != origin.scope.app
        || format!("{}/{}", method.app, method.session) != origin.scope.session_type
        || !arguments.is_object()
    {
        return Err(invalid_data("session method differs from captured session"));
    }
    let permit =
        tasks.methods.clone().try_acquire_owned().map_err(|_| invalid_data("session method capacity exhausted"))?;
    origin.inspect(&tasks.platform).await?;
    let handle = tasks
        .platform
        .control
        .clone()
        .prepare_session_method(request(
            PrepareSessionMethodRequest {
                claim: Some(origin.identity.clone()),
                app_id: method.app,
                session: method.session,
                method: method.name,
                arguments_json: arguments.to_string(),
                timeout_ms: 5000,
            },
            &tasks.platform.target.control.token,
        )?)
        .await
        .map_err(io::Error::other)?
        .into_inner();
    if handle.operation_id.is_empty() {
        return Err(invalid_data("missing prepared method identity"));
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(io::Error::other)?;
    let now = u64::try_from(now.as_millis()).map_err(invalid_data)?;
    let deadline =
        tokio::time::Instant::now() + Duration::from_millis(handle.deadline_ms.saturating_sub(now).min(5000));
    let mut prepared =
        Prepared { platform: tasks.platform.clone(), operation: handle.operation_id, armed: true, deadline };
    let mut result = tasks
        .platform
        .control
        .clone()
        .start_prepared_method(request(
            PreparedMethodRequest { operation_id: prepared.operation.clone() },
            &tasks.platform.target.control.token,
        )?)
        .await;
    let deadline = prepared.deadline;
    loop {
        if let Ok(response) = result {
            let result = response.into_inner();
            if result.operation_id != prepared.operation {
                return Err(invalid_data("session method operation changed"));
            }
            match phase(&result)? {
                SessionMethodPhase::Accepted if send => {
                    track_send(tasks, origin, prepared, permit);
                    return Ok(Value::Null);
                }
                SessionMethodPhase::Completed => {
                    prepared.armed = false;
                    return serde_json::from_str(&result.result_json).map_err(invalid_data);
                }
                SessionMethodPhase::Accepted => {}
                _ => return Err(invalid_data("session method failed or outcome unknown")),
            }
        }
        // An ambiguous start never allocates another handle or starts another operation.
        if tokio::time::Instant::now() >= deadline {
            return Err(invalid_data("session method outcome unknown"));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        result = tasks
            .platform
            .control
            .clone()
            .poll_prepared_method(request(
                PreparedMethodRequest { operation_id: prepared.operation.clone() },
                &tasks.platform.target.control.token,
            )?)
            .await;
    }
}
fn phase(result: &SessionMethodResult) -> io::Result<SessionMethodPhase> {
    SessionMethodPhase::try_from(result.phase).map_err(invalid_data)
}

fn track_send(tasks: &Tasks, origin: &Origin, mut prepared: Prepared, permit: tokio::sync::OwnedSemaphorePermit) {
    let connection = tasks.connection.clone();
    let invocation = tasks.invocation.clone();
    let deadline = prepared.deadline;
    let scope = origin.cancellation.clone();
    tasks.platform.cleanup.spawn(async move {
        let _permit = permit;
        let poll = async {
            loop {
                tokio::time::sleep(Duration::from_millis(25)).await;
                let Ok(request) = request(
                    PreparedMethodRequest { operation_id: prepared.operation.clone() },
                    &prepared.platform.target.control.token,
                ) else {
                    return;
                };
                let Ok(result) = prepared.platform.control.clone().poll_prepared_method(request).await else {
                    continue;
                };
                let result = result.into_inner();
                if result.operation_id != prepared.operation {
                    return;
                }
                if matches!(
                    phase(&result),
                    Ok(SessionMethodPhase::Completed | SessionMethodPhase::Cancelled | SessionMethodPhase::Failed)
                ) {
                    prepared.armed = false;
                    return;
                }
            }
        };
        tokio::select! {
            () = invocation.cancelled() => {},
            () = connection.cancelled() => {},
            () = scope.cancelled() => {},
            _ = tokio::time::timeout_at(deadline, poll) => {},
        }
    });
}
