use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

use chunk_proto::v1::{PrepareSessionMethodRequest, PreparedMethodHandle, SessionMethodPhase, SessionMethodResult};
use prost::Message;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::{Control, Error, PreparedSessionMethod, Result};

const MAX_PENDING: usize = 128;
const MAX_RECORDS: usize = 4096;
const MAX_BYTES: usize = 16 * 1024 * 1024;
const RESULT_RESERVE: usize = 52 * 1024;
const RETENTION_MS: u64 = 300_000;

#[derive(Default)]
pub(super) struct Methods {
    state: Mutex<Registry>,
    shutdown: CancellationToken,
}
#[derive(Default)]
struct Registry {
    records: BTreeMap<String, Record>,
    bytes: usize,
}
struct Record {
    prepared: Option<Arc<PreparedSessionMethod>>,
    deadline_ms: u64,
    cancel: CancellationToken,
    started: bool,
    result: Option<SessionMethodResult>,
    finished_ms: u64,
    bytes: usize,
}
impl Methods {
    fn state(&self) -> Result<MutexGuard<'_, Registry>> {
        self.state.lock().map_err(|_| Error::Unresolved("method registry poisoned"))
    }
    pub(super) fn prepare(
        &self,
        control: &Control,
        request: &PrepareSessionMethodRequest,
    ) -> Result<PreparedMethodHandle> {
        if request.encoded_len() > 56 * 1024 || request.arguments_json.len() > 48 * 1024 {
            return Err(Error::Invalid("session method request size limit"));
        }
        let mut state = self.state()?;
        state.expire();
        if self.shutdown.is_cancelled()
            || state.records.values().filter(|record| record.result.is_none()).count() >= MAX_PENDING
        {
            return Err(Error::Capacity);
        }
        let claim = request.claim.as_ref().ok_or(Error::Invalid("missing method claim"))?;
        let captured = control.capture_session(claim)?;
        if !captured.matches_declaration(&request.app_id, &request.session) {
            return Err(Error::Invalid("session method declaration target mismatch"));
        }
        let prepared = control.prepare_session_method(
            &captured,
            &request.method,
            serde_json::from_str(&request.arguments_json).map_err(|_| Error::Invalid("invalid method JSON"))?,
            Duration::from_millis(u64::from(request.timeout_ms)),
        )?;
        control.admits_method(&prepared)?;
        let bytes = prepared.retained_bytes()?.checked_add(RESULT_RESERVE).ok_or(Error::Capacity)?;
        state.make_room(bytes)?;
        let handle =
            PreparedMethodHandle { operation_id: prepared.operation_id().into(), deadline_ms: prepared.deadline_ms() };
        state.records.insert(
            handle.operation_id.clone(),
            Record {
                deadline_ms: handle.deadline_ms,
                prepared: Some(Arc::new(prepared)),
                cancel: CancellationToken::new(),
                started: false,
                result: None,
                finished_ms: 0,
                bytes,
            },
        );
        state.bytes += bytes;
        Ok(handle)
    }
    pub(super) fn start(
        self: &Arc<Self>,
        control: &Arc<Control>,
        tasks: &TaskTracker,
        id: &str,
    ) -> Result<SessionMethodResult> {
        validate_handle(id)?;
        let mut state = self.state()?;
        state.expire();
        let Some(record) = state.records.get_mut(id) else {
            return Ok(result(id, SessionMethodPhase::Unknown));
        };
        if record.started || record.result.is_some() {
            return Ok(record.snapshot(id));
        }
        if self.shutdown.is_cancelled() {
            return Ok(result(id, SessionMethodPhase::Unknown));
        }
        let prepared = record.prepared.as_ref().ok_or(Error::Invalid("missing prepared method"))?.clone();
        if control.method_runtime(prepared.target()).is_err() {
            state.finish(id, result(id, SessionMethodPhase::Cancelled));
            return Ok(result(id, SessionMethodPhase::Cancelled));
        }
        record.started = true;
        let cancellation = record.cancel.clone();
        let methods = self.clone();
        let control = control.clone();
        tasks.spawn(async move {
            let _completion = Completion { methods: methods.clone(), id: prepared.operation_id().into() };
            let result = control.run_session_method(&prepared, &cancellation, &methods.shutdown).await;
            if let Ok(mut state) = methods.state() {
                state.finish(prepared.operation_id(), result);
            }
        });
        Ok(result(id, SessionMethodPhase::Accepted))
    }
    pub(super) fn poll(&self, id: &str, cancel: bool) -> Result<SessionMethodResult> {
        validate_handle(id)?;
        let mut state = self.state()?;
        state.expire();
        let Some(record) = state.records.get_mut(id) else {
            return Ok(result(id, SessionMethodPhase::Unknown));
        };
        if cancel && record.result.is_none() {
            record.cancel.cancel();
            if !record.started {
                state.finish(id, result(id, SessionMethodPhase::Cancelled));
            }
        }
        Ok(state.records[id].snapshot(id))
    }
    pub(super) fn close(&self) {
        // Admission and task creation hold the same lock, so no new task can cross shutdown.
        if let Ok(mut state) = self.state() {
            self.shutdown.cancel();
            let unstarted: Vec<_> = state
                .records
                .iter()
                .filter(|(_, record)| !record.started && record.result.is_none())
                .map(|(id, _)| id.clone())
                .collect();
            for id in unstarted {
                state.finish(&id, result(&id, SessionMethodPhase::Cancelled));
            }
            for record in state.records.values() {
                record.cancel.cancel();
            }
        } else {
            self.shutdown.cancel();
        }
    }
}
impl Registry {
    fn expire(&mut self) {
        let now = crate::now_ms();
        self.records.retain(|_, record| {
            let expired = if record.result.is_some() {
                now.saturating_sub(record.finished_ms) >= RETENTION_MS
            } else if now >= record.deadline_ms {
                record.cancel.cancel();
                !record.started
            } else {
                false
            };
            if expired {
                self.bytes -= record.bytes;
            }
            !expired
        });
    }
    fn make_room(&mut self, additional: usize) -> Result<()> {
        if additional > MAX_BYTES {
            return Err(Error::Capacity);
        }
        while self.records.len() >= MAX_RECORDS || self.bytes + additional > MAX_BYTES {
            let retired = self
                .records
                .iter()
                .filter(|(_, record)| record.result.is_some())
                .min_by_key(|(_, record)| record.finished_ms)
                .map(|(id, _)| id.clone())
                .ok_or(Error::Capacity)?;
            self.bytes -= self.records.remove(&retired).ok_or(Error::Capacity)?.bytes;
        }
        Ok(())
    }
    fn finish(&mut self, id: &str, result: SessionMethodResult) {
        if let Some(record) = self.records.get_mut(id) {
            self.bytes -= record.bytes;
            record.bytes = result.encoded_len() + 128;
            self.bytes += record.bytes;
            record.prepared = None;
            record.result = Some(result);
            record.finished_ms = crate::now_ms();
        }
    }
}
impl Record {
    fn snapshot(&self, id: &str) -> SessionMethodResult {
        self.result.clone().unwrap_or_else(|| {
            result(
                id,
                if self.cancel.is_cancelled() { SessionMethodPhase::Unknown } else { SessionMethodPhase::Accepted },
            )
        })
    }
}
fn validate_handle(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 {
        return Err(Error::Invalid("invalid session method handle"));
    }
    Ok(())
}
fn result(id: &str, phase: SessionMethodPhase) -> SessionMethodResult {
    SessionMethodResult { operation_id: id.into(), phase: phase as i32, ..Default::default() }
}

struct Completion {
    methods: Arc<Methods>,
    id: String,
}
impl Drop for Completion {
    fn drop(&mut self) {
        if let Ok(mut state) = self.methods.state()
            && state.records.get(&self.id).is_some_and(|record| record.result.is_none())
        {
            state.finish(&self.id, result(&self.id, SessionMethodPhase::Unknown));
        }
    }
}
