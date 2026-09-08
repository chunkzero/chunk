use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use chunk_js::{Cancellation, DeploymentId, Engine, Execution, Invocation, Mode};
use chunk_store::{Operation, Revision, Storage, Write};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::{
    Error, Result,
    commit::{Committer, Job},
    reads::{Dependencies, Host, View},
    service::{Call, Command, Event, Request, Subscription, Update},
};

mod pipeline;

const MAX_DEPLOYMENTS: usize = 16;
const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_PENDING: usize = 16;
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

struct Mutation {
    operation: Operation,
    call: Call,
    waiters: Vec<Request<Update>>,
}

struct Pending {
    operation: String,
    revision: Revision,
    writes: Vec<Write>,
    bytes: usize,
}

struct Subscribed {
    id: u64,
    errored: bool,
    call: Call,
    dependencies: Dependencies,
    json: Arc<str>,
    sender: watch::Sender<Result<Update>>,
}

struct Reevaluation {
    view: Rc<View>,
    writes: Option<Vec<Write>>,
    ids: VecDeque<u64>,
}

pub(crate) struct Actor {
    recovering: bool,
    next_subscription: u64,
    reevaluations: VecDeque<Reevaluation>,
    js: Engine,
    versions: BTreeMap<DeploymentId, [u8; 32]>,
    view: Rc<View>,
    committer: Committer,
    outstanding: usize,
    mutations: BTreeMap<String, Mutation>,
    pending: VecDeque<Pending>,
    pending_bytes: usize,
    deferred: VecDeque<(Update, Request<Update>)>,
    subscriptions: Vec<Subscribed>,
    failure: Option<Error>,
}

impl Actor {
    pub fn new(store: Box<dyn Storage>, events: mpsc::Sender<Event>) -> Result<Self> {
        let (committer, snapshot) = Committer::new(store, events)?;
        Ok(Self {
            recovering: false,
            next_subscription: 0,
            reevaluations: VecDeque::new(),
            js: Engine::new()?,
            versions: BTreeMap::new(),
            view: Rc::new(View::new(snapshot)),
            committer,
            outstanding: 0,
            mutations: BTreeMap::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            deferred: VecDeque::new(),
            subscriptions: Vec::new(),
            failure: None,
        })
    }

    pub fn run(mut self, mut incoming: mpsc::Receiver<Event>, stopped: &AtomicBool) {
        loop {
            if stopped.load(Ordering::Acquire) && self.outstanding == 0 {
                break;
            }
            let event = if self.reevaluations.is_empty() {
                incoming.blocking_recv()
            } else {
                match incoming.try_recv() {
                    Ok(event) => Some(event),
                    Err(mpsc::error::TryRecvError::Empty) => {
                        self.reevaluate_one();
                        continue;
                    }
                    Err(mpsc::error::TryRecvError::Disconnected) => None,
                }
            };
            let Some(event) = event else {
                break;
            };
            self.subscriptions
                .retain(|subscription| !subscription.sender.is_closed());
            match event {
                Event::Request(command) => {
                    if stopped.load(Ordering::Acquire) {
                        command.reject(Error::Closed);
                    } else if let Some(error) = &self.failure {
                        command.reject(error.clone());
                    } else {
                        self.request(*command);
                    }
                }
                Event::Committed { operation, result } => {
                    self.outstanding -= 1;
                    self.committed(&operation, result);
                }
                Event::Wake => {}
            }
            self.reevaluate_one();
        }
        incoming.close();
        while let Ok(event) = incoming.try_recv() {
            if let Event::Request(command) = event {
                command.reject(Error::Closed);
            }
        }
        self.fail(&Error::Closed);
        // Close the reply path before joining the committer, including early shutdown.
        drop(incoming);
    }

    fn request(&mut self, command: Command) {
        match command {
            Command::Register {
                id,
                source,
                limits,
                reply,
            } => {
                let result = if reply.cancellation.is_cancelled() {
                    Err(Error::Cancelled)
                } else if self.versions.len() >= MAX_DEPLOYMENTS {
                    Err(Error::Busy)
                } else {
                    let digest = Sha256::digest(source.as_bytes()).into();
                    self.js
                        .register(id.clone(), source, limits)
                        .map_err(Error::from)
                        .map(|()| {
                            self.versions.insert(id, digest);
                        })
                };
                reply.finish(result);
            }
            Command::Release { id, reply } => {
                let result = if self.subscriptions.iter().any(|s| s.call.deployment == id)
                    || self.mutations.values().any(|m| m.call.deployment == id)
                {
                    Err(Error::Busy)
                } else {
                    self.versions.remove(&id);
                    Ok(self.js.release(&id))
                };
                reply.finish(result);
            }
            Command::Query { call, reply } => {
                let result = self.evaluate(&call, Mode::Query, self.view.clone(), &reply.cancellation);
                match result {
                    Ok((execution, dependencies)) => {
                        let independent = self
                            .pending
                            .iter()
                            .all(|pending| !dependencies.affected(&pending.writes));
                        let update = Update {
                            revision: if independent {
                                self.view.base.revision
                            } else {
                                self.view.revision
                            },
                            json: execution.value.into(),
                        };
                        if update.revision <= self.view.base.revision {
                            reply.finish(Ok(update));
                        } else {
                            self.deferred.push_back((update, reply));
                        }
                    }
                    Err(error) => reply.finish(Err(error)),
                }
            }
            Command::Mutate { operation, call, reply } => self.mutate(operation, call, reply),
            Command::Subscribe { call, reply } => {
                if self.subscriptions.len() >= MAX_SUBSCRIPTIONS {
                    reply.finish(Err(Error::Busy));
                    return;
                }
                let view = Rc::new(View::new(self.view.base.clone()));
                match self.evaluate(&call, Mode::Query, view, &reply.cancellation) {
                    Ok((execution, dependencies)) => {
                        let json: Arc<str> = execution.value.into();
                        let (sender, receiver) = watch::channel(Ok(Update {
                            revision: self.view.base.revision,
                            json: json.clone(),
                        }));
                        self.next_subscription += 1;
                        self.subscriptions.push(Subscribed {
                            id: self.next_subscription,
                            errored: false,
                            call,
                            dependencies,
                            json,
                            sender,
                        });
                        reply.finish(Ok(Subscription::new(receiver)));
                    }
                    Err(error) => reply.finish(Err(error)),
                }
            }
        }
    }

    fn evaluate(
        &mut self,
        call: &Call,
        mode: Mode,
        view: Rc<View>,
        cancellation: &Cancellation,
    ) -> Result<(Execution, Dependencies)> {
        let (execution, dependencies) = self.evaluate_traced(call, mode, view, cancellation);
        execution.map(|execution| (execution, dependencies))
    }

    fn evaluate_traced(
        &mut self,
        call: &Call,
        mode: Mode,
        view: Rc<View>,
        cancellation: &Cancellation,
    ) -> (Result<Execution>, Dependencies) {
        let trace = Rc::new(RefCell::new(Dependencies::default()));
        let host = Host {
            view,
            trace: trace.clone(),
        };
        let execution = self
            .js
            .execute(
                &call.deployment,
                Invocation {
                    export: call.function.clone(),
                    arguments: call.arguments.clone(),
                    caller: call.caller.clone(),
                    mode,
                },
                Box::new(host),
                cancellation,
            )
            .map_err(Error::from);
        let dependencies = std::mem::take(&mut *trace.borrow_mut());
        (execution, dependencies)
    }

    fn send(&mut self, job: Job) -> Result<()> {
        self.committer.send(job)?;
        self.outstanding += 1;
        Ok(())
    }

    fn publish(&mut self, writes: &[Write]) {
        let batch = Reevaluation {
            view: Rc::new(View::new(self.view.base.clone())),
            writes: Some(writes.to_vec()),
            ids: self.subscriptions.iter().map(|subscription| subscription.id).collect(),
        };
        if self.reevaluations.len() == 2 {
            // Slow watches coalesce to the latest durable snapshot. Reevaluating all
            // watches avoids retaining an unbounded history of invalidating writes.
            let next = self.reevaluations.back_mut().expect("queued batch");
            *next = Reevaluation { writes: None, ..batch };
        } else {
            self.reevaluations.push_back(batch);
        }
    }

    fn reevaluate_one(&mut self) {
        let Some(batch) = self.reevaluations.front_mut() else {
            return;
        };
        let Some(id) = batch.ids.pop_front() else {
            self.reevaluations.pop_front();
            return;
        };
        let Some(index) = self.subscriptions.iter().position(|subscription| subscription.id == id) else {
            return;
        };
        if batch
            .writes
            .as_ref()
            .is_some_and(|writes| !self.subscriptions[index].dependencies.affected(writes))
        {
            return;
        }
        let view = batch.view.clone();
        let mut subscription = self.subscriptions.remove(index);
        if subscription.sender.is_closed() {
            return;
        }
        let (result, dependencies) =
            self.evaluate_traced(&subscription.call, Mode::Query, view.clone(), &Cancellation::default());
        subscription.dependencies = dependencies;
        match result {
            Ok(execution) => {
                if subscription.errored || execution.value != subscription.json.as_ref() {
                    subscription.json = execution.value.into();
                    let _ = subscription.sender.send_replace(Ok(Update {
                        revision: view.revision,
                        json: subscription.json.clone(),
                    }));
                }
                subscription.errored = false;
            }
            Err(error) => {
                subscription.errored = true;
                let _ = subscription.sender.send_replace(Err(error));
            }
        }
        self.subscriptions.push(subscription);
    }

    fn reset_pending(&mut self, error: &Error) {
        for (_, mutation) in std::mem::take(&mut self.mutations) {
            for reply in mutation.waiters {
                reply.finish(Err(error.clone()));
            }
        }
        for (_, reply) in self.deferred.drain(..) {
            reply.finish(Err(error.clone()));
        }
        self.pending.clear();
        self.pending_bytes = 0;
        self.view = Rc::new(View::new(self.view.base.clone()));
    }

    fn fail(&mut self, error: &Error) {
        self.failure = Some(error.clone());
        self.reset_pending(error);
        self.reevaluations.clear();
        for subscription in self.subscriptions.drain(..) {
            let _ = subscription.sender.send_replace(Err(error.clone()));
        }
        self.pending.clear();
        self.pending_bytes = 0;
        self.view = Rc::new(View::new(self.view.base.clone()));
    }
}
