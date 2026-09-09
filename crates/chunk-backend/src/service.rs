use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
};

use chunk_js::{Cancellation, DeploymentId, Json, Limits};
use chunk_store::{Revision, Snapshot, Storage};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc as queue, oneshot, watch};

use crate::{Error, Result, actor::Actor};

const REQUESTS: usize = 64;

#[derive(Clone)]
pub struct Call {
    pub deployment: DeploymentId,
    pub function: String,
    pub arguments: Json,
    pub caller: Json,
}

impl Call {
    fn validate(&self) -> Result<()> {
        if self.function.is_empty() || self.function.len() > 128 {
            return Err(Error::Invalid("function name"));
        }
        for value in [&self.arguments, &self.caller] {
            if value.as_str().len() > 1024 * 1024 {
                return Err(Error::Invalid("input limit"));
            }
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

pub(crate) struct Request<T> {
    pub cancellation: Cancellation,
    reply: oneshot::Sender<Result<T>>,
    _permit: OwnedSemaphorePermit,
}

impl<T> Request<T> {
    pub fn finish(self, result: Result<T>) {
        let _ = self.reply.send(result);
    }
}

pub(crate) enum Command {
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
        call: Call,
        reply: Request<Subscription>,
    },
}

impl Command {
    pub fn reject(self, error: Error) {
        match self {
            Self::Register { reply, .. } => reply.finish(Err(error)),
            Self::Release { reply, .. } => reply.finish(Err(error)),
            Self::Query { reply, .. } | Self::Mutate { reply, .. } => reply.finish(Err(error)),
            Self::Subscribe { reply, .. } => reply.finish(Err(error)),
        }
    }
}

pub(crate) enum Event {
    Request(Box<Command>),
    Committed {
        operation: String,
        result: Result<(Update, Snapshot)>,
    },
    Wake,
}

struct Owner {
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
    /// Storage must hold the environment's exclusive writer authority and have its
    /// schema installed. Construction waits for the initial snapshot and engine.
    /// # Errors
    /// Reports thread, snapshot or JS engine initialization failures.
    pub fn new(store: Box<dyn Storage>) -> Result<Self> {
        chunk_js::Engine::init_platform();
        let (events, incoming) = queue::channel(REQUESTS);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let outgoing = events.clone();
        let (ready, initialized) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("chunk-environment".into())
            .spawn(move || match Actor::new(store, outgoing) {
                Ok(actor) => {
                    if ready.send(Ok(())).is_ok() {
                        actor.run(incoming, &stop);
                    }
                }
                Err(error) => {
                    let _ = ready.send(Err(error));
                }
            })?;
        let backend = Self(Arc::new(Owner {
            events,
            slots: Arc::new(Semaphore::new(REQUESTS)),
            stopped,
            thread: Some(thread),
        }));
        initialized.recv().map_err(|_| Error::Closed)??;
        Ok(backend)
    }

    async fn submit<T>(&self, make: impl FnOnce(Request<T>) -> Command) -> Result<T> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        let permit = self.0.slots.clone().try_acquire_owned().map_err(|_| Error::Busy)?;
        let cancellation = Cancellation::default();
        let _cancel = CancelOnDrop(cancellation.clone());
        let (reply, response) = oneshot::channel();
        let request = Request {
            cancellation,
            reply,
            _permit: permit,
        };
        self.0
            .events
            .try_send(Event::Request(Box::new(make(request))))
            .map_err(|error| match error {
                queue::error::TrySendError::Full(_) => Error::Busy,
                queue::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        response.await.map_err(|_| Error::Closed)?
    }

    /// # Errors
    /// Rejects duplicates, invalid bundles, execution budgets and excess resident versions.
    pub async fn register(&self, id: DeploymentId, source: String, limits: Limits) -> Result<()> {
        if source.len() > 4 * 1024 * 1024 {
            return Err(Error::Invalid("source limit"));
        }
        self.submit(|reply| Command::Register {
            id,
            source,
            limits,
            reply,
        })
        .await
    }

    /// # Errors
    /// Rejects release while mutations or live subscriptions still reference the version.
    pub async fn release(&self, id: DeploymentId) -> Result<bool> {
        self.submit(|reply| Command::Release { id, reply }).await
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
        call.validate()?;
        self.submit(|reply| Command::Subscribe { call, reply }).await
    }
}

struct CancelOnDrop(Cancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct Subscription {
    receiver: watch::Receiver<Result<Update>>,
    initial: bool,
}

impl Subscription {
    pub(crate) fn new(receiver: watch::Receiver<Result<Update>>) -> Self {
        Self {
            receiver,
            initial: true,
        }
    }

    /// Returns the initial result, then waits for changed results or an error.
    /// # Errors
    /// Reports execution, commit failure or backend shutdown.
    pub async fn next(&mut self) -> Result<Update> {
        if !std::mem::take(&mut self.initial) {
            self.receiver.changed().await.map_err(|_| Error::Closed)?;
        }
        self.receiver.borrow_and_update().clone()
    }
}
