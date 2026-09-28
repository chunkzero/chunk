use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use bytes::Bytes;
use chunk_contract::{Deployment, DomainManifest, FunctionKind, validate_wire_value};
#[cfg(test)]
use chunk_js::Limits;
use chunk_js::{Cancellation, DeploymentId, Json};
use chunk_store::{Revision, Snapshot, Storage};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc as queue, oneshot, watch};

use crate::{
    ActionEffects, ActionHandle, ActionId, ActionStatus, Error, Result, SendBudget,
    actor::Actor,
    limits::{EngineQueue, Limit, REQUEST_BYTES, REQUEST_OVERHEAD, action_bytes, send_bytes},
};

/// Admission is bounded by `limits::REQUEST_BYTES`; this only bounds the channel's own memory.
const EVENTS: usize = 65_536;
/// Query engines beyond this rarely pay for their memory: one engine per core, up to four.
const MAX_READERS: usize = 4;

fn default_readers() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get).min(MAX_READERS)
}

#[derive(Clone)]
pub struct Call {
    pub deployment: DeploymentId,
    pub function: String,
    pub arguments: Json,
    pub caller: Json,
}

impl Call {
    pub(crate) fn validate(&self) -> Result<()> {
        self.validate_limit(256)
    }

    /// Input bytes a request for this call charges against admission.
    pub(crate) fn bytes(&self) -> usize {
        self.function.len() + self.arguments.as_str().len() + self.caller.as_str().len()
    }

    pub(crate) fn validate_limit(&self, name_limit: usize) -> Result<()> {
        if self.function.is_empty() || self.function.len() > name_limit {
            return Err(Error::Invalid("function name"));
        }
        for value in [&self.arguments, &self.caller] {
            if value.as_str().len() > 1024 * 1024 {
                return Err(Error::Invalid("input limit"));
            }
            validate_wire_value(&serde_json::from_str(value.as_str())?).map_err(Error::Invalid)?;
        }
        Ok(())
    }
}

pub(crate) fn validate_operation(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 256 || id.starts_with(crate::system::OPERATION_PREFIX) {
        return Err(Error::Invalid("operation identity"));
    }
    Ok(())
}

/// A result whose revision has reached durable storage.
#[derive(Debug, Clone)]
pub struct Update {
    pub revision: Revision,
    pub json: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct GroupUpdate {
    pub revision: Revision,
    /// Each query's JSON result, shared with every other group subscribed to the same query.
    pub results: Vec<Result<Bytes>>,
    /// Changes whenever `results` do.
    pub version: u64,
}

/// The revisions at which a group's published update holds: from the latest evaluation of any of its results until
/// the revision before a commit invalidates one, or the durable revision while none did.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hold {
    pub version: u64,
    pub from: Revision,
    pub until: Option<Revision>,
}

pub(crate) struct Request<T> {
    pub cancellation: Cancellation,
    pub queued: crate::timing::Timer,
    reply: oneshot::Sender<Result<T>>,
    permit: OwnedSemaphorePermit,
    /// Request memory held by a reply retained until its revision is durable.
    retained: Option<OwnedSemaphorePermit>,
}

impl<T> Request<T> {
    pub(crate) fn new(
        cancellation: Cancellation,
        reply: oneshot::Sender<Result<T>>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self { cancellation, queued: crate::timing::Timer::start(), reply, permit, retained: None }
    }

    pub fn retain(&mut self, permit: OwnedSemaphorePermit) {
        self.retained = Some(permit);
    }

    pub fn finish(self, result: Result<T>) {
        let _ = self.reply.send(result);
    }

    /// Replies with `result`, returning the request's admission for the caller to hold while its work runs on.
    pub fn finish_holding(self, result: Result<T>) -> OwnedSemaphorePermit {
        let _ = self.reply.send(result);
        self.permit
    }
}

pub(crate) enum Command {
    Catalog {
        id: DeploymentId,
        scope: crate::CommandScope,
        caller: Json,
        reply: Request<crate::CommandCatalog>,
    },
    Suggest {
        id: DeploymentId,
        request: crate::CommandSuggestionRequest,
        caller: Json,
        reply: Request<Vec<String>>,
    },
    DomainManifest {
        id: DeploymentId,
        reply: Request<Option<DomainManifest>>,
    },
    Functions {
        id: DeploymentId,
        reply: Request<BTreeMap<String, FunctionKind>>,
    },
    JobStatus {
        id: String,
        caller: Json,
        reply: Request<chunk_store::Job>,
    },
    WakeHandoff {
        reply: Request<chunk_store::WakeHandoff>,
    },
    JobControl {
        command: chunk_store::JobCommand,
        reply: Request<chunk_store::Jobs>,
    },
    PrepareAction {
        reply: Request<ActionId>,
    },
    ActionIdentity {
        id: ActionId,
        reply: Request<crate::ActionIdentity>,
    },
    OwnedIdentity {
        id: ActionId,
        owner: [u8; 32],
        request: Option<crate::CommandRequest>,
        reply: Request<crate::CommandIdentity>,
    },
    StartAction {
        purpose: crate::commands::Purpose,
        id: ActionId,
        call: Call,
        reply: Request<ActionHandle>,
    },
    ActionStatus {
        id: ActionId,
        caller: Json,
        reply: Request<ActionStatus>,
    },
    Deploy {
        deployment: Arc<Deployment>,
        reply: Request<()>,
    },
    #[cfg(test)]
    Register {
        id: DeploymentId,
        source: String,
        limits: Limits,
        reply: Request<()>,
    },
    Release {
        id: DeploymentId,
        reply: Request<bool>,
    },
    CheckDeployment {
        id: DeploymentId,
        reply: Request<()>,
    },
    Deployments {
        reply: Request<Vec<DeploymentId>>,
    },
    Query {
        call: Call,
        reply: Request<Update>,
    },
    Mutate {
        operation: String,
        call: Call,
        reply: Request<Update>,
    },
    Subscribe {
        calls: Vec<Call>,
        reply: Request<GroupSubscription>,
    },
}

impl Command {
    pub fn reject(self, error: Error) {
        match self {
            Self::Catalog { reply, .. } => reply.finish(Err(error)),
            Self::Suggest { reply, .. } => reply.finish(Err(error)),
            Self::DomainManifest { reply, .. } => reply.finish(Err(error)),
            Self::Functions { reply, .. } => reply.finish(Err(error)),

            Self::JobStatus { reply, .. } => reply.finish(Err(error)),
            Self::WakeHandoff { reply } => reply.finish(Err(error)),
            Self::JobControl { reply, .. } => reply.finish(Err(error)),
            Self::PrepareAction { reply } => reply.finish(Err(error)),
            Self::ActionIdentity { reply, .. } => reply.finish(Err(error)),
            Self::OwnedIdentity { reply, .. } => reply.finish(Err(error)),
            Self::StartAction { reply, .. } => reply.finish(Err(error)),
            Self::ActionStatus { reply, .. } => reply.finish(Err(error)),
            Self::Deploy { reply, .. } | Self::CheckDeployment { reply, .. } => reply.finish(Err(error)),
            #[cfg(test)]
            Self::Register { reply, .. } => reply.finish(Err(error)),
            Self::Release { reply, .. } => reply.finish(Err(error)),
            Self::Deployments { reply } => reply.finish(Err(error)),
            Self::Query { reply, .. } | Self::Mutate { reply, .. } => reply.finish(Err(error)),
            Self::Subscribe { reply, .. } => reply.finish(Err(error)),
        }
    }
}

pub(crate) enum Event {
    Scheduled {
        command: chunk_store::JobCommand,
        result: Result<chunk_store::Jobs>,
    },
    ActionPlatform {
        id: ActionId,
        sequence: u32,
        request: Json,
        reply: Request<Arc<str>>,
    },
    ActionFinished {
        id: ActionId,
        result: Result<Arc<str>>,
    },
    ActionTransaction {
        id: ActionId,
        sequence: u32,
        mode: chunk_js::Mode,
        function: String,
        arguments: Json,
        reply: Request<Update>,
    },
    Prepared {
        operation: String,
        result: Result<chunk_store::RetryContext>,
    },
    Request {
        command: Box<Command>,
        admitted: std::time::Instant,
    },
    Committed {
        operation: String,
        result: Result<(Update, Snapshot, Option<chunk_store::Jobs>)>,
    },
    Activated {
        result: Result<chunk_store::Snapshot>,
    },
    Released {
        result: Result<bool>,
    },
    Evaluated(Box<crate::actor::Evaluated>),
    /// System commits up to `revision` that precede every app commit not yet acknowledged,
    /// and a snapshot that includes them.
    System {
        count: u64,
        revision: Revision,
        snapshot: Result<Snapshot>,
    },
    /// The commit thread failed and commits nothing more.
    Failed,
    Wake,
}

struct Owner {
    environment: String,
    events: queue::Sender<Event>,
    memory: Arc<Semaphore>,
    send: SendBudget,
    queue: Arc<EngineQueue>,
    lane: Arc<crate::system::Lane>,
    stopped: Arc<AtomicBool>,
    thread: std::sync::Mutex<Option<JoinHandle<()>>>,
}

impl Owner {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // A full queue already guarantees the engine will wake and see shutdown.
        let _ = self.events.try_send(Event::Wake);
        // Joining under the lock makes concurrent callers wait for the same drain.
        let mut thread = self.thread.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(thread) = thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Cloneable ingress to one environment engine. Admission is bounded, including
/// requests waiting for durability. Dropping the last handle drains accepted
/// commits and joins both threads; use a blocking task when dropping from async code.
#[derive(Clone)]
pub struct Backend(Arc<Owner>);

/// Request memory [`Backend::charge_request`] charged for a payload its caller holds until it hands the payload to a
/// request, such as [`Backend::start_command`], which takes the charge over.
pub struct RequestCharge(OwnedSemaphorePermit);

impl Backend {
    /// Holds ingress without an actor so tests can inspect admission before dequeue.
    #[cfg(test)]
    pub(crate) fn held_ingress() -> (Self, queue::Receiver<Event>, Arc<Semaphore>) {
        let (events, incoming) = queue::channel(EVENTS);
        let memory = Arc::new(Semaphore::new(REQUEST_BYTES));
        let backend = Self(Arc::new(Owner {
            environment: "test".into(),
            events,
            memory: memory.clone(),
            send: SendBudget::new(send_bytes()),
            queue: Arc::default(),
            lane: Arc::default(),
            stopped: Arc::default(),
            thread: std::sync::Mutex::new(None),
        }));
        (backend, incoming, memory)
    }

    /// Storage must hold the environment's exclusive writer authority. Construction waits for the initial snapshot and engine.
    /// # Errors
    /// Reports thread, snapshot or JS engine initialization failures.
    pub fn new(environment: String, store: Box<dyn Storage>) -> Result<Self> {
        Self::with_action_effects(environment.clone(), store, ActionEffects::new(environment)?)
    }

    /// Construct with immutable host-provided action grants. Grants bind the exact
    /// environment and deployment; queries and mutations gain no external effects.
    /// # Errors
    /// Reports invalid scope, thread, snapshot or JS initialization failures.
    pub fn with_action_effects(environment: String, store: Box<dyn Storage>, effects: ActionEffects) -> Result<Self> {
        Self::start(environment, store, effects, default_readers(), action_bytes())
    }

    /// Starts with exactly `readers` query engines.
    #[cfg(test)]
    pub(crate) fn with_readers(environment: String, store: Box<dyn Storage>, readers: usize) -> Result<Self> {
        let effects = ActionEffects::new(environment.clone())?;
        Self::start(environment, store, effects, readers, crate::limits::ACTION_BYTES)
    }

    /// Starts with a live-action budget of exactly `action_bytes`, rather than one sized from the machine's memory.
    /// # Errors
    /// Reports what [`Self::with_action_effects`] does.
    pub fn with_action_bytes(
        environment: String,
        store: Box<dyn Storage>,
        effects: ActionEffects,
        action_bytes: usize,
    ) -> Result<Self> {
        Self::start(environment, store, effects, default_readers(), action_bytes)
    }

    /// The request memory budget admission charges.
    #[cfg(test)]
    pub(crate) fn request_memory(&self) -> Arc<Semaphore> {
        self.0.memory.clone()
    }

    fn start(
        environment: String,
        store: Box<dyn Storage>,
        effects: ActionEffects,
        readers: usize,
        action_bytes: usize,
    ) -> Result<Self> {
        effects.validate_environment(&environment)?;
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("environment identity"));
        }
        chunk_js::Engine::init_platform();
        let (events, incoming) = queue::channel(EVENTS);
        let engine_queue = Arc::new(EngineQueue::default());
        let dequeued = engine_queue.clone();
        let memory = Arc::new(Semaphore::new(REQUEST_BYTES));
        let retained = memory.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let outgoing = events.clone();
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("chunk-environment".into()).spawn(move || {
            match Actor::new(store, outgoing, effects, action_bytes, readers, dequeued, retained) {
                Ok(actor) => {
                    if ready.send(Ok(actor.lane())).is_ok() {
                        actor.run(incoming, &stop);
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            }
        })?;
        let lane = match initialized.recv().map_err(|_| Error::Closed).and_then(|lane| lane) {
            Ok(lane) => lane,
            Err(error) => {
                let _ = thread.join();
                return Err(error);
            }
        };
        Ok(Self(Arc::new(Owner {
            environment,
            events,
            memory,
            send: SendBudget::new(send_bytes()),
            queue: engine_queue,
            lane,
            stopped,
            thread: std::sync::Mutex::new(Some(thread)),
        })))
    }

    /// Stops admitting requests, drains accepted commits and joins both threads,
    /// even while other handles exist; their later requests fail with
    /// [`Error::Closed`]. Blocks, so call it from a blocking task in async code.
    pub fn stop(&self) {
        self.0.stop();
    }

    #[must_use]
    pub fn environment(&self) -> &str {
        &self.0.environment
    }

    /// The native system module's handle to this environment's store.
    #[must_use]
    pub fn system(&self) -> crate::System {
        crate::System::new(self.clone(), self.0.lane.clone())
    }

    /// Validates and durably retains a deployment before enabling its functions.
    /// Activation installs additive tables/indexes at a commit barrier. Restart reloads retained bundles.
    /// # Errors
    /// Rejects incompatible metadata, invalid JS, pending commits or retention limits.
    pub async fn deploy(&self, deployment: Deployment) -> Result<()> {
        deployment.validate().map_err(Error::Invalid)?;
        let bytes = serde_json::to_vec(&deployment)?.len();
        self.submit_sized(bytes, |reply| Command::Deploy { deployment: Arc::new(deployment), reply }).await
    }

    pub(crate) async fn submit<T>(&self, make: impl FnOnce(Request<T>) -> Command) -> Result<T> {
        self.submit_sized(0, make).await
    }

    /// Admits a request carrying `bytes` of input until it replies.
    pub(crate) async fn submit_sized<T>(&self, bytes: usize, make: impl FnOnce(Request<T>) -> Command) -> Result<T> {
        let RequestCharge(permit) = self.charge_request(bytes)?;
        self.submit_charged(permit, make).await
    }

    /// Charges `bytes` of payload its caller holds against request admission, until the charge drops or a request
    /// takes it over.
    /// # Errors
    /// Reports exhausted request memory.
    pub fn charge_request(&self, bytes: usize) -> Result<RequestCharge> {
        let cost = u32::try_from(REQUEST_OVERHEAD + bytes).map_err(|_| Limit::RequestMemory.exceeded())?;
        let permit = self.0.memory.clone().try_acquire_many_owned(cost).map_err(|_| Limit::RequestMemory.exceeded())?;
        Ok(RequestCharge(permit))
    }

    /// Bytes of request memory held by charges and by requests until they reply.
    #[must_use]
    pub fn request_bytes(&self) -> usize {
        REQUEST_BYTES - self.0.memory.available_permits()
    }

    /// The budget outgoing sync messages are charged against, sized from the machine's memory.
    #[must_use]
    pub fn send_budget(&self) -> &SendBudget {
        &self.0.send
    }

    /// `charge`, grown to admit a request carrying `bytes` of input.
    pub(crate) fn cover(&self, RequestCharge(mut permit): RequestCharge, bytes: usize) -> Result<OwnedSemaphorePermit> {
        if !Arc::ptr_eq(permit.semaphore(), &self.0.memory) {
            return Err(Error::Invalid("the charge belongs to another backend"));
        }
        let more = (REQUEST_OVERHEAD + bytes).saturating_sub(permit.num_permits());
        if more > 0 {
            let more = u32::try_from(more).map_err(|_| Limit::RequestMemory.exceeded())?;
            let more = self.0.memory.clone().try_acquire_many_owned(more);
            permit.merge(more.map_err(|_| Limit::RequestMemory.exceeded())?);
        }
        Ok(permit)
    }

    /// Submits a request whose admission `permit` holds until it replies.
    pub(crate) async fn submit_charged<T>(
        &self,
        permit: OwnedSemaphorePermit,
        make: impl FnOnce(Request<T>) -> Command,
    ) -> Result<T> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        self.0.queue.enter()?;
        let cancellation = Cancellation::default();
        let (reply, response) = oneshot::channel();
        let request = Request::new(cancellation.clone(), reply, permit);
        let event = Event::Request { command: Box::new(make(request)), admitted: std::time::Instant::now() };
        self.0.events.try_send(event).map_err(|error| {
            self.0.queue.leave();
            match error {
                queue::error::TrySendError::Full(_) => Limit::EngineQueue.exceeded(),
                queue::error::TrySendError::Closed(_) => Error::Closed,
            }
        })?;
        let mut cancel = CancelOnDrop { cancellation, events: Some(&self.0.events) };
        let result = response.await;
        cancel.events = None;
        result.map_err(|_| Error::Closed)?
    }

    /// # Errors
    /// Rejects duplicates, invalid bundles, execution budgets and excess resident versions.
    #[cfg(test)]
    pub async fn register(&self, id: DeploymentId, source: String, limits: Limits) -> Result<()> {
        if source.len() > 4 * 1024 * 1024 {
            return Err(Error::Invalid("source limit"));
        }
        self.submit(|reply| Command::Register { id, source, limits, reply }).await
    }

    /// # Errors
    /// Rejects release while mutations or live subscriptions still reference the version.
    pub async fn release(&self, id: DeploymentId) -> Result<bool> {
        self.submit(|reply| Command::Release { id, reply }).await
    }

    /// The deployments resident beside each other, including one being released.
    /// # Errors
    /// Reports an unavailable service.
    pub async fn deployments(&self) -> Result<Vec<DeploymentId>> {
        self.submit(|reply| Command::Deployments { reply }).await
    }

    /// Checks that the exact deployment is resident and accepting calls.
    /// # Errors
    /// Reports unknown deployments, release in progress or unavailable service.
    pub async fn check_deployment(&self, id: DeploymentId) -> Result<()> {
        self.submit(|reply| Command::CheckDeployment { id, reply }).await
    }

    /// Prepares the identity of one action or hook, valid until it starts or 24 hours pass. Allocate once per
    /// business invocation and reuse the ID after a lost reply.
    /// # Errors
    /// Reports a full retention budget, exhausted invocation identities or unavailable service.
    pub async fn allocate_action_id(&self) -> Result<ActionId> {
        self.submit(|reply| Command::PrepareAction { reply }).await
    }

    /// Acceptance retains the deployment and starts at most one action for this
    /// identity. Duplicate requests attach to the same scope/result. Acceptance
    /// and results are ephemeral; stale or retired identities are never restarted.
    /// # Errors
    /// Rejects unknown identities, mismatched requests, inaccessible functions or
    /// exhausted capacity. Dropping an acceptance future cancels its scope.
    pub async fn start_action(&self, id: ActionId, call: Call) -> Result<ActionHandle> {
        call.validate()?;
        let bytes = id.incarnation.len() + call.bytes();
        self.submit_sized(bytes, |reply| Command::StartAction {
            purpose: crate::commands::Purpose::Function,
            id,
            call,
            reply,
        })
        .await
    }

    /// Looks up native hook descriptors in the exact retained deployment.
    /// # Errors
    /// Rejects unknown or releasing deployments.
    pub async fn domain_manifest(&self, id: DeploymentId) -> Result<Option<DomainManifest>> {
        self.submit(|reply| Command::DomainManifest { id, reply }).await
    }

    /// The kind of each public function in the exact retained deployment.
    /// # Errors
    /// Rejects unknown or releasing deployments.
    pub async fn functions(&self, id: DeploymentId) -> Result<BTreeMap<String, FunctionKind>> {
        self.submit(|reply| Command::Functions { id, reply }).await
    }

    /// Starts the hook `call` names under an identity from [`Self::allocate_action_id`], as
    /// [`Self::start_action`] starts an action.
    /// # Errors
    /// Rejects identities this backend didn't allocate, mismatched requests, unknown hooks, untrusted hook context or
    /// exhausted capacity. Dropping an acceptance future cancels its scope.
    pub async fn start_hook(&self, id: ActionId, call: Call) -> Result<ActionHandle> {
        call.validate_limit(512)?;
        let bytes = id.incarnation.len() + call.bytes();
        self.submit_sized(bytes, |reply| Command::StartAction {
            purpose: crate::commands::Purpose::Hook,
            id,
            call,
            reply,
        })
        .await
    }

    /// Resolves an identity from [`Self::allocate_action_id`] without consulting any deployment, so a retry can route
    /// to the action or hook it started even after the deployment's release.
    /// # Errors
    /// Returns unknown for an identity from another incarnation, one never issued, or one whose outcome is gone.
    pub async fn action_identity(&self, id: ActionId) -> Result<crate::ActionIdentity> {
        let bytes = id.incarnation.len();
        self.submit_sized(bytes, |reply| Command::ActionIdentity { id, reply }).await
    }

    /// Look up retained status using the original caller authority.
    /// # Errors
    /// Returns unknown after restart, retention expiry or a caller mismatch.
    pub async fn action_status(&self, id: ActionId, caller: Json) -> Result<ActionStatus> {
        let bytes = id.incarnation.len() + caller.as_str().len();
        self.submit_sized(bytes, |reply| Command::ActionStatus { id, caller, reply }).await
    }

    /// Reads a durable job record using its originating caller authority.
    /// # Errors
    /// Rejects unknown jobs, caller mismatches and unavailable service.
    pub async fn job(&self, id: String, caller: Json) -> Result<chunk_store::Job> {
        let bytes = id.len() + caller.as_str().len();
        self.submit_sized(bytes, |reply| Command::JobStatus { id, caller, reply }).await
    }

    /// Forget a terminal record. This does not reverse earlier effects.
    /// # Errors
    /// Rejects live jobs, caller mismatches and persistence failures.
    pub async fn forget_job(&self, id: String, caller: Json) -> Result<()> {
        let bytes = id.len() + caller.as_str().len();
        let caller = serde_json::from_str(caller.as_str())?;
        self.submit_sized(bytes, |reply| Command::JobControl {
            command: chunk_store::JobCommand::Forget { id, caller },
            reply,
        })
        .await
        .map(|_| ())
    }

    /// Host-adapter handoff: durably install this exact alarm before acknowledging it.
    /// # Errors
    /// Reports unavailable service.
    pub async fn wake_handoff(&self) -> Result<chunk_store::WakeHandoff> {
        self.submit(|reply| Command::WakeHandoff { reply }).await
    }

    /// Acknowledge only after the host adapter durably installed (or cleared) the alarm.
    /// # Errors
    /// Rejects stale generation/time, unsupported storage or persistence failures.
    pub async fn acknowledge_wake(&self, generation: u64, due_at: Option<i64>) -> Result<chunk_store::WakeHandoff> {
        self.submit(|reply| Command::JobControl {
            command: chunk_store::JobCommand::AcknowledgeWake { generation, due_at },
            reply,
        })
        .await
        .map(|jobs| jobs.wake)
    }

    /// Reads the current view, waiting for durability if it includes staged writes.
    /// # Errors
    /// Reports admission, execution, cancellation and persistence failures.
    pub async fn query(&self, call: Call) -> Result<Update> {
        call.validate()?;
        self.submit_sized(call.bytes(), |reply| Command::Query { call, reply }).await
    }

    /// Commits once for this operation identity and request. A lost/cancelled reply
    /// can follow a durable commit; retry using the same identity to recover it.
    /// # Errors
    /// Reports mismatched identities, admission, execution and commit failures.
    pub async fn mutate(&self, operation: String, call: Call) -> Result<Update> {
        validate_operation(&operation)?;
        call.validate()?;
        let bytes = operation.len() + call.bytes();
        self.submit_sized(bytes, |reply| Command::Mutate { operation, call, reply }).await
    }

    /// Subscribes against the durable snapshot. Slow consumers coalesce updates;
    /// every delivered result belongs to a durable revision.
    /// # Errors
    /// Reports invalid queries, execution failures or the subscription capacity limit.
    pub async fn subscribe(&self, call: Call) -> Result<Subscription> {
        Ok(Subscription(self.subscribe_group(vec![call]).await?))
    }

    /// Publishes all queries against one durable snapshot. Query errors retain
    /// dependencies and may recover on later updates without closing the group.
    /// # Errors
    /// Rejects invalid calls, non-query functions and oversized groups.
    pub async fn subscribe_group(&self, calls: Vec<Call>) -> Result<GroupSubscription> {
        if calls.is_empty() || calls.len() > 16 {
            return Err(Error::Invalid("query group limit"));
        }
        let mut input = 0;
        for call in &calls {
            call.validate()?;
            input += call.arguments.as_str().len() + call.caller.as_str().len();
        }
        if input > 1024 * 1024 {
            return Err(Error::Invalid("query group input limit"));
        }
        let bytes = calls.iter().map(Call::bytes).sum();
        self.submit_sized(bytes, |reply| Command::Subscribe { calls, reply }).await
    }
}

/// Cancels a request once its caller stops waiting. A caller that leaves before the reply also wakes the actor, so work
/// queued for it, such as an action start waiting for capacity, releases its admission.
struct CancelOnDrop<'a> {
    cancellation: Cancellation,
    events: Option<&'a queue::Sender<Event>>,
}

impl Drop for CancelOnDrop<'_> {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(events) = self.events {
            // A full queue already guarantees the actor will wake.
            let _ = events.try_send(Event::Wake);
        }
    }
}

pub struct GroupSubscription {
    receiver: watch::Receiver<Result<GroupUpdate>>,
    initial: bool,
    progress: Progress,
}

impl GroupSubscription {
    pub(crate) fn new(
        receiver: watch::Receiver<Result<GroupUpdate>>,
        hold: watch::Receiver<Option<Hold>>,
        durable: watch::Receiver<Revision>,
    ) -> Self {
        Self { receiver, initial: true, progress: Progress { hold, durable } }
    }

    /// Follows the later durable revisions at which this group's results hold.
    #[must_use]
    pub fn progress(&self) -> Progress {
        self.progress.clone()
    }

    /// Returns the initial result, then waits for changed results or an error.
    /// # Errors
    /// Reports execution, commit failure or backend shutdown.
    pub async fn next(&mut self) -> Result<GroupUpdate> {
        if !std::mem::take(&mut self.initial) {
            self.receiver.changed().await.map_err(|_| Error::Closed)?;
        }
        self.receiver.borrow_and_update().clone()
    }
}

/// Tells a group's subscriber how far its results hold at later durable revisions, without waking it for each commit
/// unless it waits.
#[derive(Clone)]
pub struct Progress {
    hold: watch::Receiver<Option<Hold>>,
    durable: watch::Receiver<Revision>,
}

impl Progress {
    /// The latest durable revision at which the update with `version` holds, if it still holds at any.
    pub fn holds(&mut self, version: u64) -> Option<Revision> {
        // The durable revision is announced after the holds it affects, so it is read first.
        let durable = *self.durable.borrow_and_update();
        let hold = (*self.hold.borrow_and_update()).filter(|hold| hold.version == version)?;
        let last = hold.until.map_or(durable, |until| until.min(durable));
        (last >= hold.from).then_some(last)
    }

    /// Waits until the durable revision passes `revision`.
    pub async fn durable_after(&mut self, revision: Revision) {
        if self.durable.wait_for(|durable| *durable > revision).await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Waits for a commit, or a change in how far the results hold, since [`Self::holds`] last looked.
    pub async fn changed(&mut self) {
        let changed = tokio::select! {
            changed = self.durable.changed() => changed,
            changed = self.hold.changed() => changed,
        };
        if changed.is_err() {
            // The group or backend is gone, which its results report.
            std::future::pending::<()>().await;
        }
    }
}

pub struct Subscription(GroupSubscription);

impl Subscription {
    /// Returns the first result and subsequent changes. Data-dependent errors may
    /// recover on the next call; storage failure and shutdown close the subscription.
    /// # Errors
    /// Reports execution, storage failure or shutdown.
    pub async fn next(&mut self) -> Result<Update> {
        let mut group = self.0.next().await?;
        let json = group.results.remove(0)?;
        let json = std::str::from_utf8(&json).map_err(|_| Error::Invalid("query result encoding"))?;
        Ok(Update { revision: group.revision, json: json.into() })
    }
}
