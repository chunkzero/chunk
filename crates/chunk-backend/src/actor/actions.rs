use std::{
    collections::BTreeMap,
    rc::Rc,
    sync::{Arc, Weak},
    thread::JoinHandle,
    time::Duration,
};

use chunk_contract::{FunctionKind, validate_wire_value};
use chunk_js::{ActionInvocation, Cancellation, DeploymentId, Engine, Json, Limits, Mode};
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, mpsc, watch};

use super::Actor;
use crate::{
    ActionHandle, ActionId, ActionStatus, Error, Result,
    actions::{Host, Scope},
    service::{Call, Event, Request, Update},
};

const MAX_LIVE: usize = 8;
const MAX_RECORDS: usize = 32;

struct Record {
    operation_prefix: String,
    call: Call,
    fingerprint: [u8; 32],
    status: watch::Sender<ActionStatus>,
    scope: Weak<Scope>,
    cancellation: Cancellation,
    worker: Option<JoinHandle<()>>,
}

pub(super) struct Actions {
    incarnation: String,
    retired: u64,
    records: BTreeMap<ActionId, Record>,
    events: mpsc::Sender<Event>,
    slots: Arc<Semaphore>,
    external_slots: Arc<Semaphore>,
    effects: crate::ActionEffects,
}

impl Actions {
    pub fn new(events: mpsc::Sender<Event>, incarnation: String, effects: crate::ActionEffects) -> Self {
        Self {
            incarnation,
            retired: 0,
            records: BTreeMap::new(),
            events,
            slots: Arc::new(Semaphore::new(32)),
            external_slots: Arc::new(Semaphore::new(8)),
            effects,
        }
    }

    pub fn status(&self, id: &ActionId, caller: &Json) -> Result<ActionStatus> {
        self.records
            .get(id)
            .filter(|record| record.call.caller.as_str() == caller.as_str())
            .map(|record| record.status.borrow().clone())
            .ok_or(Error::ActionOutcomeUnknown)
    }

    pub fn references(&self, id: &DeploymentId) -> bool {
        self.records.values().any(|record| record.worker.is_some() && &record.call.deployment == id)
    }

    pub fn cancel(&self) {
        for record in self.records.values() {
            record.cancellation.cancel();
        }
    }

    pub fn capacity(&self) -> bool {
        self.records.values().filter(|record| record.worker.is_some()).count() < MAX_LIVE
    }

    pub fn job_id(&self, job: &chunk_store::Job) -> ActionId {
        ActionId { incarnation: format!("{}:job:{}", self.incarnation, job.id), sequence: u64::from(job.attempt) }
    }

    fn admit(&mut self, id: &ActionId, trusted: bool) -> Result<()> {
        if !trusted && (id.incarnation != self.incarnation || id.sequence <= self.retired) {
            return Err(Error::ActionOutcomeUnknown);
        }
        if self.records.values().filter(|record| record.worker.is_some()).count() >= MAX_LIVE {
            return Err(Error::Busy);
        }
        if self.records.len() >= MAX_RECORDS {
            let retired = self
                .records
                .iter()
                .find(|(_, record)| record.worker.is_none())
                .map(|(id, _)| id.clone())
                .ok_or(Error::Busy)?;
            if retired.incarnation == self.incarnation {
                self.retired = self.retired.max(retired.sequence);
            }
            self.records.remove(&retired);
        }
        if !trusted && id.sequence <= self.retired {
            return Err(Error::ActionOutcomeUnknown);
        }
        Ok(())
    }
}

impl Drop for Actions {
    fn drop(&mut self) {
        self.cancel();
        for record in self.records.values_mut() {
            if let Some(worker) = record.worker.take() {
                let _ = worker.join();
            }
        }
    }
}

impl Actor {
    pub(super) fn start_action(&mut self, id: ActionId, call: Call, reply: Request<ActionHandle>) {
        if reply.cancellation.is_cancelled() {
            reply.finish(Err(Error::Cancelled));
            return;
        }
        let result = self.launch_action(id, call, None);
        reply.finish(result);
    }

    pub(super) fn launch_action(
        &mut self,
        id: ActionId,
        mut call: Call,
        durable_identity: Option<String>,
    ) -> Result<ActionHandle> {
        self.normalize_scoped_call(&mut call, durable_identity.is_some())?;
        let deployment = self.versions.get(&call.deployment).and_then(Option::as_ref).ok_or(Error::Contract)?.clone();
        let function = deployment.functions.get(&call.function).ok_or(Error::Unknown)?.clone();
        if function.kind != FunctionKind::Action {
            return Err(Error::Contract);
        }
        let fingerprint: [u8; 32] = Sha256::digest(serde_json::to_vec(&(
            "action-v1",
            call.deployment.as_str(),
            &call.function,
            call.arguments.as_str(),
            call.caller.as_str(),
        ))?)
        .into();
        if let Some(record) = self.actions.records.get(&id) {
            if record.fingerprint != fingerprint {
                return Err(Error::OperationMismatch);
            }
            let scope = record.scope.upgrade().unwrap_or_else(|| Arc::new(Scope(record.cancellation.clone())));
            return Ok(ActionHandle { id, status: record.status.subscribe(), scope });
        }
        self.actions.admit(&id, durable_identity.is_some())?;
        let seed = durable_identity.as_ref().map_or(id.sequence, |identity| {
            u64::from_be_bytes(Sha256::digest(identity.as_bytes())[..8].try_into().expect("digest prefix"))
        });
        let operation_prefix = durable_identity.clone().unwrap_or_else(|| format!("action/{id}"));
        let invocation_identity = durable_identity.unwrap_or_else(|| id.to_string());
        let cancellation = Cancellation::default();
        let scope = Arc::new(Scope(cancellation.clone()));
        let (status, receiver) = watch::channel(ActionStatus::Running);
        let events = self.actions.events.clone();
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        let grants = self.actions.effects.grants(&call.deployment);
        let host = Host {
            effects: Arc::new(crate::effects::ScopedEffects {
                invocation: invocation_identity.clone(),
                grants: grants.clone(),
                slots: self.actions.external_slots.clone(),
                cancellation: cancellation.clone(),
                deadline,
            }),
            id: id.clone(),
            events: events.clone(),
            slots: self.actions.slots.clone(),
            cancellation: cancellation.clone(),
        };
        let invocation = ActionInvocation {
            id: invocation_identity,
            export: function.export,
            arguments: call.arguments.clone(),
            caller: call.caller.clone(),
            timestamp: self.view.base.timestamp,
            seed,
            deadline,
        };
        let worker_id = id.clone();
        let worker_cancellation = cancellation.clone();
        let worker = std::thread::Builder::new().name("chunk-action".into()).spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<Arc<str>> {
                    let mut engine = Engine::new()?;
                    let deployment_id = DeploymentId::new(&deployment.id)?;
                    engine.register(deployment_id.clone(), deployment.source.clone(), Limits::default())?;
                    let execution = engine.execute_action(&deployment_id, invocation, Rc::new(host), &worker_cancellation)?;
                    for log in execution.logs {
                        tracing::info!(target: "chunk_backend::console", invocation = %worker_id, level = log.level, message = grants.redact(log.message));
                    }
                    let mut value = serde_json::from_str(&execution.value)?;
                    function.result.normalize_api(&mut value);
                    validate_wire_value(&value).map_err(Error::Invalid)?;
                    if !function.result.accepts(&value) { return Err(Error::Contract); }
                    Ok(serde_json::to_string(&value)?.into())
                })).unwrap_or(Err(Error::ActionOutcomeUnknown));
                let result = result.map_err(|error| match error {
                    Error::JavaScript(ref inner) => match inner.as_ref() {
                        chunk_js::Error::JavaScript(message) => Error::from(chunk_js::Error::JavaScript(grants.redact(message.clone()))),
                        _ => error,
                    },
                    _ => error,
                });
                worker_cancellation.cancel();
                let _ = events.blocking_send(Event::ActionFinished { id: worker_id, result });
            })?;
        self.actions.records.insert(
            id.clone(),
            Record {
                operation_prefix,
                call,
                fingerprint,
                status,
                scope: Arc::downgrade(&scope),
                cancellation,
                worker: Some(worker),
            },
        );
        Ok(ActionHandle { id, status: receiver, scope })
    }

    pub(super) fn finish_action(&mut self, id: &ActionId, result: Result<Arc<str>>) {
        if let Some(record) = self.actions.records.get_mut(id) {
            record.cancellation.cancel();
            record.status.send_replace(ActionStatus::Finished(result));
            if let Some(worker) = record.worker.take() {
                let _ = worker.join();
            }
        }
    }

    pub(super) fn action_transaction(
        &mut self,
        id: &ActionId,
        sequence: u32,
        mode: Mode,
        function: String,
        arguments: Json,
        reply: Request<Update>,
    ) {
        let Some(record) = self.actions.records.get(id) else {
            reply.finish(Err(Error::ActionOutcomeUnknown));
            return;
        };
        if record.worker.is_none() || record.cancellation.is_cancelled() || reply.cancellation.is_cancelled() {
            reply.finish(Err(Error::Cancelled));
            return;
        }
        let operation = format!("{}/{sequence}", record.operation_prefix);
        let mut call = Call { function, arguments, ..record.call.clone() };
        if let Err(error) = call.validate().and_then(|()| self.normalize_scoped_call(&mut call, true)) {
            reply.finish(Err(error));
            return;
        }
        match mode {
            Mode::Query => self.query(&call, reply),
            Mode::Mutation => self.mutate(operation, call, reply),
        }
    }
}
