use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use chunk_contract::{Deployment, Function, FunctionKind, Visibility, validate_wire_value};
use chunk_js::{Cancellation, DeploymentId, Engine, Execution, Invocation, Limits, Mode};
use chunk_store::{Operation, Revision, Storage, Write};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use crate::{
    Error, Result,
    commit::{Committer, Job},
    reads::{Change, Dependencies, Host, View},
    service::{Call, Command, Event, GroupUpdate, Request, Update},
};

mod pipeline;
mod subscriptions;

const MAX_DEPLOYMENTS: usize = 16;
const MAX_SUBSCRIPTIONS: usize = 64;
const MAX_PENDING: usize = 16;
const MAX_PENDING_BYTES: usize = 32 * 1024 * 1024;

struct Mutation {
    operation: Operation,
    context: Option<chunk_store::RetryContext>,
    call: Call,
    waiters: Vec<Request<Update>>,
}

struct Pending {
    operation: String,
    revision: Revision,
    writes: Vec<Write>,
    changes: Vec<Change>,
    bytes: usize,
}

struct Subscribed {
    id: u64,
    calls: Vec<Call>,
    dependencies: Dependencies,
    results: Vec<Result<Arc<str>>>,
    sender: watch::Sender<Result<GroupUpdate>>,
}

struct Reevaluation {
    view: Rc<View>,
    changes: Option<Vec<Change>>,
    ids: VecDeque<u64>,
}

pub(crate) struct Actor {
    recovering: bool,
    next_subscription: u64,
    reevaluations: VecDeque<Reevaluation>,
    js: Engine,
    versions: BTreeMap<DeploymentId, Option<Arc<Deployment>>>,
    deploying: Option<(Arc<Deployment>, Request<()>)>,
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
        let (committer, snapshot, deployments) = Committer::new(store, events)?;
        let mut js = Engine::new()?;
        let mut versions = BTreeMap::new();
        for deployment in deployments {
            deployment.validate().map_err(Error::Invalid)?;
            Self::schema_ready(&deployment, snapshot.schema())?;
            let id = DeploymentId::new(&deployment.id)?;
            js.register(id.clone(), deployment.source.clone(), Limits::default())?;
            versions.insert(id, Some(Arc::new(deployment)));
        }
        Ok(Self {
            recovering: false,
            next_subscription: 0,
            reevaluations: VecDeque::new(),
            js,
            versions,
            deploying: None,
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
                Event::Prepared { operation, result } => {
                    self.outstanding -= 1;
                    self.prepared(&operation, result);
                }
                Event::Committed { operation, result } => {
                    self.outstanding -= 1;
                    self.committed(&operation, result);
                }
                Event::Retained { result } => {
                    self.outstanding -= 1;
                    if let Some((deployment, reply)) = self.deploying.take() {
                        let id = DeploymentId::new(&deployment.id).expect("validated deployment");
                        match result {
                            Ok(()) => {
                                self.versions.insert(id, Some(deployment));
                                reply.finish(Ok(()));
                            }
                            Err(error) => {
                                self.js.release(&id);
                                if !error.is_rejected_commit() {
                                    self.fail(&Error::CommitFailed);
                                }
                                reply.finish(Err(error));
                            }
                        }
                    }
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
            Command::Deploy { deployment, reply } => {
                if reply.cancellation.is_cancelled() {
                    reply.finish(Err(Error::Cancelled));
                    return;
                }
                let result = self.start_deployment(&deployment);
                match result {
                    Ok(true) => {
                        self.deploying = Some((deployment, reply));
                    }
                    Ok(false) => reply.finish(Ok(())),
                    Err(error) => reply.finish(Err(error)),
                }
            }
            #[cfg(test)]
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
                    self.js
                        .register(id.clone(), source, limits)
                        .map_err(Error::from)
                        .map(|()| {
                            self.versions.insert(id, None);
                        })
                };
                reply.finish(result);
            }
            Command::Release { id, reply } => {
                let result = if self
                    .subscriptions
                    .iter()
                    .any(|s| s.calls.iter().any(|c| c.deployment == id))
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
                            .all(|pending| !dependencies.affected(&pending.changes));
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
            Command::Subscribe { calls, reply } => self.subscribe(calls, reply),
        }
    }

    fn schema_ready(deployment: &Deployment, installed: &chunk_contract::DatabaseSchema) -> Result<()> {
        for (name, table) in &deployment.tables {
            let current = installed.get(name).ok_or(Error::Contract)?;
            if table
                .fields
                .iter()
                .any(|(name, field)| current.fields.get(name) != Some(field))
                || table
                    .indexes
                    .iter()
                    .any(|(name, fields)| current.indexes.get(name) != Some(fields))
            {
                return Err(Error::Contract);
            }
        }
        Ok(())
    }

    fn start_deployment(&mut self, deployment: &Arc<Deployment>) -> Result<bool> {
        let id = DeploymentId::new(&deployment.id)?;
        if let Some(existing) = self.versions.get(&id) {
            return if existing.as_deref() == Some(deployment.as_ref()) {
                Ok(false)
            } else {
                Err(Error::Contract)
            };
        }
        if self.outstanding != 0 || self.deploying.is_some() || self.versions.len() >= MAX_DEPLOYMENTS {
            return Err(Error::Busy);
        }
        Self::schema_ready(deployment, self.view.base.schema())?;
        self.js
            .register(id.clone(), deployment.source.clone(), Limits::default())?;
        if let Err(error) = self.send(Job::Retain {
            deployment: deployment.clone(),
        }) {
            self.js.release(&id);
            return Err(error);
        }
        Ok(true)
    }

    fn resolve(&self, call: &Call, mode: Mode) -> Result<Option<Function>> {
        let version = self.versions.get(&call.deployment).ok_or(Error::Unknown)?;
        let Some(deployment) = version else {
            return Ok(None);
        };
        let function = deployment.functions.get(&call.function).ok_or(Error::Unknown)?;
        if function.visibility != Visibility::Public {
            return Err(Error::Unknown);
        }
        let kind = match mode {
            Mode::Query => FunctionKind::Query,
            Mode::Mutation => FunctionKind::Mutation,
        };
        if function.kind != kind {
            return Err(Error::Contract);
        }
        let arguments = serde_json::from_str(call.arguments.as_str())?;
        if !function.arguments.accepts(&arguments) {
            return Err(Error::Contract);
        }
        Ok(Some(function.clone()))
    }

    fn evaluate(
        &mut self,
        call: &Call,
        mode: Mode,
        view: Rc<View>,
        cancellation: &Cancellation,
    ) -> Result<(Execution, Dependencies)> {
        let (execution, dependencies) = self.evaluate_traced(call, mode, view, cancellation, None);
        execution.map(|execution| (execution, dependencies))
    }

    fn evaluate_traced(
        &mut self,
        call: &Call,
        mode: Mode,
        view: Rc<View>,
        cancellation: &Cancellation,
        context: Option<(i64, u64)>,
    ) -> (Result<Execution>, Dependencies) {
        let function = match self.resolve(call, mode) {
            Ok(function) => function,
            Err(error) => return (Err(error), Dependencies::default()),
        };
        let trace = Rc::new(RefCell::new(Dependencies::default()));
        let (timestamp, seed) = context.unwrap_or_else(|| {
            (view.base.timestamp, {
                u64::from_be_bytes(
                    Sha256::digest(call.function.as_bytes())[..8]
                        .try_into()
                        .expect("digest prefix"),
                )
            })
        });
        let host = Host {
            view,
            trace: trace.clone(),
            contract: self.versions.get(&call.deployment).cloned().flatten(),
            budget: crate::reads::read_budget(),
        };
        let execution = self
            .js
            .execute(
                &call.deployment,
                Invocation {
                    export: function
                        .as_ref()
                        .map_or_else(|| call.function.clone(), |f| f.export.clone()),
                    arguments: call.arguments.clone(),
                    caller: call.caller.clone(),
                    mode,
                    timestamp,
                    seed,
                },
                Box::new(host),
                cancellation,
            )
            .map_err(Error::from)
            .and_then(|execution| {
                for log in &execution.logs {
                    tracing::info!(target: "chunk_backend::console", deployment = call.deployment.as_str(), function = call.function, level = log.level, message = log.message);
                }
                let value = serde_json::from_str(&execution.value)?;
                validate_wire_value(&value).map_err(Error::Invalid)?;
                if function.as_ref().is_some_and(|f| !f.result.accepts(&value)) {
                    return Err(Error::Contract);
                }
                Ok(execution)
            });
        let dependencies = std::mem::take(&mut *trace.borrow_mut());
        (execution, dependencies)
    }

    fn send(&mut self, job: Job) -> Result<()> {
        self.committer.send(job)?;
        self.outstanding += 1;
        Ok(())
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
