use std::{
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{Arc, Weak},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use chunk_contract::{Deployment, Function, FunctionKind, validate_wire_value};
use chunk_js::{ActionInvocation, Cancellation, DeploymentId, Engine, Json, Limits, Mode};
use sha2::{Digest, Sha256};
use tokio::sync::{Semaphore, mpsc, watch};

use super::Actor;
use crate::{
    ActionHandle, ActionId, ActionIdentity, ActionStatus, CommandIdentity, CommandRequest, Error, Result,
    actions::{Host, Scope},
    limits::{Limit, QUEUE_WAIT, RETAINED_BYTES},
    service::{Call, Event, Request, Update},
};

/// How long a finished action's outcome stays retained at most, and a prepared identity nothing started stays valid.
const RETENTION: Duration = Duration::from_hours(24);
/// Retained bytes each record or prepared identity charges beyond its call and result.
const ENTRY_BYTES: usize = 256;

struct Record {
    operation_prefix: String,
    /// A running command's binding, dropped once it finishes.
    command: Option<Arc<crate::commands::CommandBinding>>,
    /// Who started a command, and what they asked for.
    owner: Option<crate::commands::Owner>,
    call: Call,
    fingerprint: [u8; 32],
    hook: bool,
    writable: bool,
    status: watch::Sender<ActionStatus>,
    scope: Weak<Scope>,
    cancellation: Cancellation,
    worker: Option<JoinHandle<()>>,
    /// Bytes this record retains.
    bytes: usize,
}

/// A start waiting for a live action to finish.
struct Waiting {
    id: ActionId,
    call: Call,
    purpose: crate::commands::Purpose,
    reply: Request<ActionHandle>,
    since: Instant,
}

pub(super) struct Actions {
    incarnation: String,
    /// Live actions the heap budget admits, each reserving its engine's heap limit.
    pub limit: usize,
    /// Starts the budget can't admit yet, oldest first; at most [`Self::limit`] wait.
    waiting: VecDeque<Waiting>,
    next: u64,
    /// Identities `prepare` issued that nothing started yet, by sequence, with when each expires.
    prepared: BTreeMap<u64, Instant>,
    records: BTreeMap<ActionId, Record>,
    /// Retained finished records, oldest first, with when each expires.
    finished: VecDeque<(Instant, ActionId)>,
    /// Bytes prepared identities and records retain. Finished records are evicted, oldest first, to keep it within
    /// [`RETAINED_BYTES`]; prepared identities and live records alone may not exceed it.
    retained: usize,
    events: mpsc::Sender<Event>,
    slots: Arc<Semaphore>,
    external_slots: Arc<Semaphore>,
    pub effects: crate::ActionEffects,
    /// Every live action is in flight here, from launch until its worker ends.
    pub activity: chunk_service::Activity,
}

impl Actions {
    pub fn new(events: mpsc::Sender<Event>, incarnation: String, effects: crate::ActionEffects, budget: usize) -> Self {
        let limit = budget / Limits::default().heap_bytes;
        Self {
            incarnation,
            limit,
            waiting: VecDeque::new(),
            next: 1,
            prepared: BTreeMap::new(),
            records: BTreeMap::new(),
            finished: VecDeque::new(),
            retained: 0,
            events,
            // Per admitted action, four transaction or platform calls and one HTTP effect may be in flight.
            slots: Arc::new(Semaphore::new(4 * limit)),
            external_slots: Arc::new(Semaphore::new(limit)),
            effects,
            activity: chunk_service::Activity::default(),
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
        self.records.values().filter(|record| record.worker.is_some()).count() < self.limit
    }

    /// Queues a start the budget can't admit yet. It's refused once queued starts wait too long or fill the queue.
    fn wait(&mut self, waiting: Waiting) {
        self.waiting.retain(|waiting| !waiting.reply.cancellation.is_cancelled());
        if self.waiting.len() >= self.limit
            || self.waiting.front().is_some_and(|front| front.since.elapsed() > QUEUE_WAIT)
        {
            waiting.reply.finish(Err(Limit::ActionMemory.exceeded()));
        } else {
            self.waiting.push_back(waiting);
        }
    }

    /// The effects an invocation started now reaches, with the secrets it keeps until it ends. Hooks read variables
    /// only, and can't fetch.
    fn host(
        &self,
        id: &ActionId,
        invocation: &str,
        cancellation: &Cancellation,
        deadline: std::time::Instant,
        purpose: &crate::commands::Purpose,
        hook: bool,
    ) -> Host {
        Host {
            effects: Arc::new(crate::effects::ScopedEffects {
                invocation: invocation.to_owned(),
                fetcher: (!hook).then(|| self.effects.fetcher.clone()),
                slots: self.external_slots.clone(),
                cancellation: cancellation.clone(),
                deadline,
            }),
            id: id.clone(),
            events: self.events.clone(),
            slots: self.slots.clone(),
            cancellation: cancellation.clone(),
            moves: matches!(purpose, crate::commands::Purpose::Function).then(|| self.effects.moves.clone()),
            secrets: if hook { Arc::default() } else { self.effects.secrets() },
        }
    }

    pub fn refuse_waiting(&mut self, error: &Error) {
        for waiting in self.waiting.drain(..) {
            waiting.reply.finish(Err(error.clone()));
        }
    }

    /// The `:job:` suffix keeps job identities outside the client-allocated incarnation, so `admit`
    /// only accepts them on the trusted durable path and `start_action` can never claim or restart
    /// one. Nothing parses the suffix back out.
    pub fn job_id(&self, job: &chunk_store::Job) -> ActionId {
        ActionId { incarnation: format!("{}:job:{}", self.incarnation, job.id), sequence: u64::from(job.attempt) }
    }

    /// Issues the identity of one action, valid until it starts or [`RETENTION`] passes.
    pub fn prepare(&mut self) -> Result<ActionId> {
        self.expire();
        let sequence = self.next;
        let next = sequence.checked_add(1).ok_or(Error::Invalid("action identity exhausted"))?;
        self.make_room(ENTRY_BYTES)?;
        self.next = next;
        self.retained += ENTRY_BYTES;
        self.prepared.insert(sequence, Instant::now() + RETENTION);
        Ok(ActionId { incarnation: self.incarnation.clone(), sequence })
    }

    /// Forgets prepared identities and finished records older than [`RETENTION`].
    fn expire(&mut self) {
        let now = Instant::now();
        while let Some(entry) = self.prepared.first_entry()
            && *entry.get() <= now
        {
            entry.remove();
            self.retained -= ENTRY_BYTES;
        }
        while self.finished.front().is_some_and(|(expiry, _)| *expiry <= now) {
            self.evict_oldest();
        }
    }

    /// Evicts the oldest finished records until `bytes` more fit the budget, which fails once prepared identities and
    /// live records alone leave no room.
    fn make_room(&mut self, bytes: usize) -> Result<()> {
        while self.retained + bytes > RETAINED_BYTES {
            if !self.evict_oldest() {
                return Err(Limit::Retention.exceeded());
            }
        }
        Ok(())
    }

    fn evict_oldest(&mut self) -> bool {
        let Some((_, id)) = self.finished.pop_front() else { return false };
        if let Some(record) = self.records.remove(&id) {
            self.retained -= record.bytes;
        }
        true
    }

    /// Resolves an untrusted `id` without consulting any deployment: its record while its action runs or its outcome is
    /// retained, or `None` while it's prepared and unused. An identity from another incarnation, never issued, or whose
    /// outcome is gone is unknown.
    fn resolve(&self, id: &ActionId) -> Result<Option<&Record>> {
        if id.incarnation != self.incarnation {
            return Err(Error::ActionOutcomeUnknown);
        }
        if let Some(record) = self.records.get(id) {
            return Ok(Some(record));
        }
        if self.prepared.contains_key(&id.sequence) { Ok(None) } else { Err(Error::ActionOutcomeUnknown) }
    }

    pub fn identity(&mut self, id: &ActionId) -> Result<ActionIdentity> {
        self.expire();
        Ok(match self.resolve(id)? {
            None => ActionIdentity::Unused,
            Some(record) if record.hook => ActionIdentity::Hook,
            Some(_) => ActionIdentity::Action,
        })
    }

    pub fn command_identity(
        &mut self,
        id: &ActionId,
        owner: &[u8; 32],
        request: Option<CommandRequest>,
    ) -> Result<CommandIdentity> {
        self.expire();
        Ok(match self.resolve(id)? {
            None => CommandIdentity::Unused,
            Some(record) => match &record.owner {
                Some(started) if started.credential != *owner => CommandIdentity::Foreign,
                Some(started) if request.is_none_or(|request| request == started.request) => {
                    CommandIdentity::Started(record.status.borrow().clone())
                }
                _ => CommandIdentity::Other,
            },
        })
    }

    /// Admits a record of `bytes` under `id`, which [`Self::resolve`] found prepared unless it's `trusted`, consuming
    /// its preparation.
    fn admit(&mut self, id: &ActionId, trusted: bool, bytes: usize) -> Result<()> {
        if !self.capacity() {
            return Err(Limit::ActionMemory.exceeded());
        }
        let released = if trusted { 0 } else { ENTRY_BYTES };
        self.make_room(bytes - released)?;
        if !trusted {
            self.prepared.remove(&id.sequence);
        }
        self.retained = self.retained - released + bytes;
        Ok(())
    }
}

/// Identifies a request as sent, so only a retry repeating it replays its outcome.
fn fingerprint(purpose: &crate::commands::Purpose, call: &Call) -> Result<[u8; 32]> {
    let request = (
        "action-v1",
        purpose.name(),
        purpose.owner().map(|owner| (owner.credential, owner.request.digest())),
        call.deployment.as_str(),
        &call.function,
        call.arguments.as_str(),
        call.caller.as_str(),
    );
    Ok(Sha256::digest(serde_json::to_vec(&request)?).into())
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

/// Runs an action of `deployment` on an engine of its own and checks its result against `result`. What its JavaScript
/// wrote to `console`, and the message of an error it threw, have `secrets` redacted.
fn run_action(
    deployment: &Deployment,
    result: &chunk_contract::Schema,
    secrets: &crate::Secrets,
    invocation: ActionInvocation,
    host: Host,
    id: &ActionId,
    cancellation: &Cancellation,
) -> Result<Arc<str>> {
    let run = || -> Result<Arc<str>> {
        let mut engine = Engine::new()?;
        let deployment_id = DeploymentId::new(&deployment.id)?;
        engine.register(deployment_id.clone(), deployment.source.clone(), Limits::default())?;
        let execution = engine.execute_action(&deployment_id, invocation, Rc::new(host), cancellation)?;
        for log in execution.logs {
            console!(log.level.as_str(), invocation = %id, message = secrets.redact(log.message));
        }
        let mut value = serde_json::from_str(&execution.value)?;
        result.normalize_api(&mut value);
        validate_wire_value(&value).map_err(Error::Invalid)?;
        if !result.accepts(&value) {
            return Err(Error::Contract);
        }
        Ok(serde_json::to_string(&value)?.into())
    };
    run().map_err(|error| match error {
        Error::JavaScript(ref inner) => match inner.as_ref() {
            chunk_js::Error::JavaScript(message) => {
                Error::from(chunk_js::Error::JavaScript(secrets.redact(message.clone())))
            }
            _ => error,
        },
        _ => error,
    })
}

impl Actor {
    pub(super) fn start_action(
        &mut self,
        id: ActionId,
        call: Call,
        purpose: crate::commands::Purpose,
        reply: Request<ActionHandle>,
    ) {
        if reply.cancellation.is_cancelled() {
            reply.finish(Err(Error::Cancelled));
            return;
        }
        match self.launch_action(id.clone(), call.clone(), None, purpose.clone(), &reply.cancellation) {
            Err(Error::Overloaded(Limit::ActionMemory)) => {
                self.actions.wait(Waiting { id, call, purpose, reply, since: Instant::now() });
            }
            result => reply.finish(result),
        }
    }

    /// Drops cancelled queued starts and resolves those that no longer need a new worker, such as duplicates of an
    /// action an earlier queued start launched or starts for a released deployment, then starts the rest in order as
    /// live actions finish. Replies go out once every start is resolved, so all callers joining an action hold its scope
    /// before any can drop it.
    pub(super) fn dispatch_actions(&mut self) {
        if self.failure.is_some() || self.actions.waiting.is_empty() {
            return;
        }
        self.actions.expire();
        let mut full = false;
        let mut replies = Vec::new();
        for waiting in std::mem::take(&mut self.actions.waiting) {
            if waiting.reply.cancellation.is_cancelled() {
                replies.push((waiting.reply, Err(Error::Cancelled)));
            } else if matches!(self.actions.resolve(&waiting.id), Ok(None))
                && self.check_deployment(&waiting.call.deployment).is_ok()
                && (full || !self.actions.capacity())
            {
                full = true;
                self.actions.waiting.push_back(waiting);
            } else {
                let Waiting { id, call, purpose, reply, .. } = waiting;
                let result = self.launch_action(id, call, None, purpose, &reply.cancellation);
                replies.push((reply, result));
            }
        }
        for (reply, result) in replies {
            reply.finish(result);
        }
    }

    pub(super) fn launch_action(
        &mut self,
        id: ActionId,
        mut call: Call,
        durable_identity: Option<String>,
        purpose: crate::commands::Purpose,
        request_cancellation: &Cancellation,
    ) -> Result<ActionHandle> {
        let hook = matches!(purpose, crate::commands::Purpose::Hook);
        self.actions.expire();
        // The identity resolves before the deployment is checked: a retained outcome outlives the deployment's release,
        // and an identity whose outcome is gone stays unknown.
        let fingerprint = fingerprint(&purpose, &call)?;
        let record =
            if durable_identity.is_some() { self.actions.records.get(&id) } else { self.actions.resolve(&id)? };
        if let Some(record) = record {
            if record.fingerprint != fingerprint {
                return Err(Error::OperationMismatch);
            }
            let scope = record.scope.upgrade().unwrap_or_else(|| Arc::new(Scope(record.cancellation.clone())));
            return Ok(ActionHandle { id, status: record.status.subscribe(), scope });
        }
        let (deployment, function, writable) =
            self.action_contract(&mut call, &purpose, durable_identity.is_some(), request_cancellation)?;
        let operation_prefix = durable_identity.clone().unwrap_or_else(|| format!("action/{id}"));
        let bytes = ENTRY_BYTES + id.incarnation.len() + operation_prefix.len() + call.bytes() + purpose.bytes();
        self.actions.admit(&id, durable_identity.is_some(), bytes)?;
        let seed = durable_identity.as_ref().map_or(id.sequence, |identity| {
            u64::from_be_bytes(Sha256::digest(identity.as_bytes())[..8].try_into().expect("digest prefix"))
        });
        let invocation_identity = durable_identity.unwrap_or_else(|| id.to_string());
        let cancellation = Cancellation::default();
        let scope = Arc::new(Scope(cancellation.clone()));
        let (status, receiver) = watch::channel(ActionStatus::Running);
        let events = self.actions.events.clone();
        let deadline =
            std::time::Instant::now() + if hook { crate::hooks::HOOK_TIMEOUT } else { Duration::from_secs(30) };
        let host = self.actions.host(&id, &invocation_identity, &cancellation, deadline, &purpose, hook);
        let invocation = ActionInvocation {
            id: invocation_identity,
            export: function.export,
            arguments: call.arguments.clone(),
            caller: call.caller.clone(),
            timestamp: self.view.base.timestamp,
            seed,
            deadline,
            env: self.actions.effects.env(&deployment),
        };
        let secrets = host.secrets.clone();
        let worker_id = id.clone();
        let worker_cancellation = cancellation.clone();
        let busy = self.actions.activity.begin();
        let worker = std::thread::Builder::new()
            .name("chunk-action".into())
            .spawn(move || {
                let run = || {
                    run_action(
                        &deployment,
                        &function.result,
                        &secrets,
                        invocation,
                        host,
                        &worker_id,
                        &worker_cancellation,
                    )
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run))
                    .unwrap_or(Err(Error::ActionOutcomeUnknown));
                worker_cancellation.cancel();
                drop(busy);
                let _ = events.blocking_send(Event::ActionFinished { id: worker_id, result });
            })
            .inspect_err(|_| self.actions.retained -= bytes)?;
        self.actions.records.insert(
            id.clone(),
            Record {
                operation_prefix,
                owner: purpose.owner(),
                command: purpose.command(),
                writable,
                call,
                fingerprint,
                hook,
                status,
                scope: Arc::downgrade(&scope),
                cancellation,
                worker: Some(worker),
                bytes,
            },
        );
        Ok(ActionHandle { id, status: receiver, scope })
    }

    fn action_contract(
        &mut self,
        call: &mut Call,
        purpose: &crate::commands::Purpose,
        trusted: bool,
        cancellation: &Cancellation,
    ) -> Result<(Arc<Deployment>, Function, bool)> {
        if matches!(purpose, crate::commands::Purpose::Function) {
            self.normalize_scoped_call(call, trusted)?;
        } else {
            self.check_deployment(&call.deployment)?;
        }
        let deployment = self.versions.get(&call.deployment).and_then(Option::as_ref).ok_or(Error::Contract)?.clone();
        let (function, writable) = match purpose {
            crate::commands::Purpose::Hook => crate::hooks::resolve(&deployment, call)?,
            crate::commands::Purpose::Function => {
                (deployment.functions.get(&call.function).ok_or(Error::Unknown)?.clone(), true)
            }
            crate::commands::Purpose::Command(binding) => {
                (self.resolve_command(&deployment, call, binding, cancellation)?, true)
            }
        };
        if function.kind != FunctionKind::Action {
            return Err(Error::Contract);
        }
        Ok((deployment, function, writable))
    }

    pub(super) fn finish_action(&mut self, id: &ActionId, result: Result<Arc<str>>) {
        let Some(record) = self.actions.records.get_mut(id) else { return };
        record.cancellation.cancel();
        let bytes = result.as_ref().map_or_else(|error| error.to_string().len(), |json| json.len());
        record.status.send_replace(ActionStatus::Finished(result));
        let Some(worker) = record.worker.take() else { return };
        let _ = worker.join();
        if let Some(binding) = record.command.take() {
            record.bytes -= binding.bytes();
            self.actions.retained -= binding.bytes();
        }
        record.bytes += bytes;
        self.actions.retained += bytes;
        self.actions.finished.push_back((Instant::now() + RETENTION, id.clone()));
        // Over budget, older outcomes give way; prepared identities and live records alone always fit.
        let _ = self.actions.make_room(0);
    }

    pub(super) fn action_platform(&mut self, id: &ActionId, sequence: u32, request: &Json, reply: Request<Arc<str>>) {
        let prepared = (|| {
            let record = self.actions.records.get(id).ok_or(Error::ActionOutcomeUnknown)?;
            if record.worker.is_none() || record.cancellation.is_cancelled() || reply.cancellation.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let binding = record.command.clone().ok_or(Error::Invalid("platform capability unavailable"))?;
            let call = record.call.clone();
            let deployment =
                self.versions.get(&call.deployment).and_then(Option::as_ref).ok_or(Error::Unknown)?.clone();
            self.command_permission(&deployment, &binding.scope, &call.function, &binding.caller, &reply.cancellation)?;
            let (request, result, receipt) = crate::commands::effects::validate(&deployment, &binding.scope, request)?;
            Ok((binding.effects.clone(), request, result, receipt))
        })();
        match prepared {
            Ok((effects, request, result, receipt)) => {
                let effect = crate::commands::PlatformEffect { sequence, request, result, receipt, reply };
                if let Err(error) = effects.try_send(effect) {
                    error.into_inner().reply.finish(Err(Error::Cancelled));
                }
            }
            Err(error) => reply.finish(Err(error)),
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
        if mode == Mode::Mutation && !record.writable {
            reply.finish(Err(Error::Invalid("hook has read-only transaction capabilities")));
            return;
        }
        let operation = format!("{}/{sequence}", record.operation_prefix);
        let command = record.command.clone();
        let original = record.call.clone();
        let mut call = Call { function, arguments, ..original.clone() };
        if let Some(binding) = command {
            let Some(deployment) = self.versions.get(&original.deployment).and_then(Option::as_ref).cloned() else {
                reply.finish(Err(Error::Unknown));
                return;
            };
            if let Err(error) = self.command_permission(
                &deployment,
                &binding.scope,
                &original.function,
                &binding.caller,
                &reply.cancellation,
            ) {
                reply.finish(Err(error));
                return;
            }
        }
        if let Err(error) = call.validate().and_then(|()| self.normalize_scoped_call(&mut call, true)) {
            reply.finish(Err(error));
            return;
        }
        match mode {
            Mode::Query => self.query(call, reply),
            Mode::Mutation => self.mutate(operation, call, reply),
        }
    }
}
