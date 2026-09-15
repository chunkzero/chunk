use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use chunk_contract::{Deployment, DomainManifest, validate_wire_value};
#[cfg(test)]
use chunk_js::Limits;
use chunk_js::{Cancellation, DeploymentId, Json};
use chunk_store::{Revision, Snapshot, Storage};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc as queue, oneshot, watch};

use crate::{ActionEffects, ActionHandle, ActionId, ActionStatus, Error, Result, actor::Actor};

const REQUESTS: usize = 64;

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

/// A result whose revision has reached durable storage.
#[derive(Debug, Clone)]
pub struct Update {
    pub revision: Revision,
    pub json: Arc<str>,
}

#[derive(Debug, Clone)]
pub struct GroupUpdate {
    pub revision: Revision,
    pub results: Vec<Result<Arc<str>>>,
}

pub(crate) struct Request<T> {
    pub cancellation: Cancellation,
    reply: oneshot::Sender<Result<T>>,
    _permit: OwnedSemaphorePermit,
}

impl<T> Request<T> {
    pub(crate) fn new(
        cancellation: Cancellation,
        reply: oneshot::Sender<Result<T>>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self { cancellation, reply, _permit: permit }
    }

    pub fn finish(self, result: Result<T>) {
        let _ = self.reply.send(result);
    }
}

pub(crate) enum Command {
    DomainManifest {
        id: DeploymentId,
        reply: Request<Option<DomainManifest>>,
    },
    StartAction {
        hook: bool,
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
            Self::DomainManifest { reply, .. } => reply.finish(Err(error)),
            Self::StartAction { reply, .. } => reply.finish(Err(error)),
            Self::ActionStatus { reply, .. } => reply.finish(Err(error)),
            Self::Deploy { reply, .. } | Self::CheckDeployment { reply, .. } => reply.finish(Err(error)),
            #[cfg(test)]
            Self::Register { reply, .. } => reply.finish(Err(error)),
            Self::Release { reply, .. } => reply.finish(Err(error)),
            Self::Query { reply, .. } | Self::Mutate { reply, .. } => reply.finish(Err(error)),
            Self::Subscribe { reply, .. } => reply.finish(Err(error)),
        }
    }
}

pub(crate) enum Event {
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
    Request(Box<Command>),
    Committed {
        operation: String,
        result: Result<(Update, Snapshot)>,
    },
    Activated {
        result: Result<chunk_store::Snapshot>,
    },
    Released {
        result: Result<bool>,
    },
    Wake,
}

struct Owner {
    environment: String,
    incarnation: String,
    action_sequence: AtomicU64,
    events: queue::Sender<Event>,
    slots: Arc<Semaphore>,
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        // A full queue already guarantees the engine will wake and see shutdown.
        let _ = self.events.try_send(Event::Wake);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Cloneable ingress to one environment engine. Admission is bounded, including
/// requests waiting for durability. Dropping the last handle drains accepted
/// commits and joins both threads; use a blocking task when dropping from async code.
#[derive(Clone)]
pub struct Backend(Arc<Owner>);

impl Backend {
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
        effects.validate_environment(&environment)?;
        if environment.is_empty() || environment.len() > 128 {
            return Err(Error::Invalid("environment identity"));
        }
        chunk_js::Engine::init_platform();
        let (events, incoming) = queue::channel(REQUESTS);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let outgoing = events.clone();
        let incarnation = uuid::Uuid::new_v4().to_string();
        let action_incarnation = incarnation.clone();
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new().name("chunk-environment".into()).spawn(move || {
            match Actor::new(store, outgoing, action_incarnation, effects) {
                Ok(actor) => {
                    if ready.send(Ok(())).is_ok() {
                        actor.run(incoming, &stop);
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            }
        })?;
        let backend = Self(Arc::new(Owner {
            environment,
            incarnation,
            action_sequence: AtomicU64::new(1),
            events,
            slots: Arc::new(Semaphore::new(REQUESTS)),
            stopped,
            thread: Some(thread),
        }));
        initialized.recv().map_err(|_| Error::Closed)??;
        Ok(backend)
    }

    #[must_use]
    pub fn environment(&self) -> &str {
        &self.0.environment
    }

    /// Validates and durably retains a deployment before enabling its functions.
    /// Activation installs additive tables/indexes at a commit barrier. Restart reloads retained bundles.
    /// # Errors
    /// Rejects incompatible metadata, invalid JS, pending commits or retention limits.
    pub async fn deploy(&self, deployment: Deployment) -> Result<()> {
        deployment.validate().map_err(Error::Invalid)?;
        self.submit(|reply| Command::Deploy { deployment: Arc::new(deployment), reply }).await
    }

    async fn submit<T>(&self, make: impl FnOnce(Request<T>) -> Command) -> Result<T> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let permit = self.0.slots.clone().try_acquire_owned().map_err(|_| Error::Busy)?;
        let cancellation = Cancellation::default();
        let _cancel = CancelOnDrop(cancellation.clone());
        let (reply, response) = oneshot::channel();
        let request = Request { cancellation, reply, _permit: permit };
        self.0.events.try_send(Event::Request(Box::new(make(request)))).map_err(|error| match error {
            queue::error::TrySendError::Full(_) => Error::Busy,
            queue::error::TrySendError::Closed(_) => Error::Closed,
        })?;
        response.await.map_err(|_| Error::Closed)?
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

    /// Checks that the exact deployment is resident and accepting calls.
    /// # Errors
    /// Reports unknown deployments, release in progress or unavailable service.
    pub async fn check_deployment(&self, id: DeploymentId) -> Result<()> {
        self.submit(|reply| Command::CheckDeployment { id, reply }).await
    }

    /// Allocate once per business invocation and reuse the ID after a lost reply.
    /// # Errors
    /// Reports exhausted invocation identities.
    pub fn allocate_action_id(&self) -> Result<ActionId> {
        let sequence = self
            .0
            .action_sequence
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| value.checked_add(1))
            .map_err(|_| Error::Invalid("action identity exhausted"))?;
        Ok(ActionId { incarnation: self.0.incarnation.clone(), sequence })
    }

    /// Acceptance retains the deployment and starts at most one action for this
    /// identity. Duplicate requests attach to the same scope/result. Acceptance
    /// and results are ephemeral; stale or retired identities are never restarted.
    /// # Errors
    /// Rejects unknown identities, mismatched requests, inaccessible functions or
    /// exhausted capacity. Dropping an acceptance future cancels its scope.
    pub async fn start_action(&self, id: ActionId, call: Call) -> Result<ActionHandle> {
        call.validate()?;
        if id.incarnation != self.0.incarnation {
            return Err(Error::ActionOutcomeUnknown);
        }
        self.submit(|reply| Command::StartAction { hook: false, id, call, reply }).await
    }

    /// Looks up native hook descriptors in the exact retained deployment.
    /// # Errors
    /// Rejects unknown or releasing deployments.
    pub async fn domain_manifest(&self, id: DeploymentId) -> Result<Option<DomainManifest>> {
        self.submit(|reply| Command::DomainManifest { id, reply }).await
    }

    pub(crate) async fn invoke_hook(&self, call: Call) -> Result<Arc<str>> {
        call.validate_limit(512)?;
        let id = self.allocate_action_id()?;
        let mut handle = self.submit(|reply| Command::StartAction { hook: true, id, call, reply }).await?;
        handle.outcome().await
    }

    /// Look up retained status using the original caller authority.
    /// # Errors
    /// Returns unknown after restart, retention expiry or a caller mismatch.
    pub async fn action_status(&self, id: ActionId, caller: Json) -> Result<ActionStatus> {
        self.submit(|reply| Command::ActionStatus { id, caller, reply }).await
    }

    /// Reads the current view, waiting for durability if it includes staged writes.
    /// # Errors
    /// Reports admission, execution, cancellation and persistence failures.
    pub async fn query(&self, call: Call) -> Result<Update> {
        call.validate()?;
        self.submit(|reply| Command::Query { call, reply }).await
    }

    /// Commits once for this operation identity and request. A lost/cancelled reply
    /// can follow a durable commit; retry using the same identity to recover it.
    /// # Errors
    /// Reports mismatched identities, admission, execution and commit failures.
    pub async fn mutate(&self, operation: String, call: Call) -> Result<Update> {
        call.validate()?;
        self.submit(|reply| Command::Mutate { operation, call, reply }).await
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
        let mut bytes = 0;
        for call in &calls {
            call.validate()?;
            bytes += call.arguments.as_str().len() + call.caller.as_str().len();
        }
        if bytes > 1024 * 1024 {
            return Err(Error::Invalid("query group input limit"));
        }
        self.submit(|reply| Command::Subscribe { calls, reply }).await
    }
}

struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct GroupSubscription {
    receiver: watch::Receiver<Result<GroupUpdate>>,
    initial: bool,
}

impl GroupSubscription {
    pub(crate) fn new(receiver: watch::Receiver<Result<GroupUpdate>>) -> Self {
        Self { receiver, initial: true }
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

pub struct Subscription(GroupSubscription);

impl Subscription {
    /// Returns the first result and subsequent changes. Data-dependent errors may
    /// recover on the next call; storage failure and shutdown close the subscription.
    /// # Errors
    /// Reports execution, storage failure or shutdown.
    pub async fn next(&mut self) -> Result<Update> {
        let mut group = self.0.next().await?;
        Ok(Update { revision: group.revision, json: group.results.remove(0)? })
    }
}
