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

mod actions;
mod deployments;
mod jobs;
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
    actions: actions::Actions,
    scheduled: jobs::Scheduled,
    recovering: bool,
    next_subscription: u64,
    reevaluations: VecDeque<Reevaluation>,
    js: Engine,
    versions: BTreeMap<DeploymentId, Option<Arc<Deployment>>>,
    deploying: Option<(Arc<Deployment>, Request<()>)>,
    releasing: Option<(DeploymentId, Request<bool>)>,
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
    pub fn new(
        store: Box<dyn Storage>,
        events: mpsc::Sender<Event>,
        incarnation: String,
        effects: crate::ActionEffects,
    ) -> Result<Self> {
        let (committer, snapshot, deployments, scheduled) = Committer::new(store, events.clone())?;
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
            scheduled: jobs::Scheduled::new(scheduled, events.clone())?,
            actions: actions::Actions::new(events, incarnation, effects),
            recovering: false,
            next_subscription: 0,
            reevaluations: VecDeque::new(),
            js,
            versions,
            deploying: None,
            releasing: None,
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
            self.subscriptions.retain(|subscription| !subscription.sender.is_closed());
            match event {
                Event::Scheduled { command, result } => {
                    self.outstanding -= 1;
                    self.scheduled(command, result);
                    self.recovering &= self.outstanding != 0;
                }
                Event::ActionFinished { id, result } => self.finish_action(&id, result),
                Event::ActionTransaction { id, sequence, mode, function, arguments, reply } => {
                    if stopped.load(Ordering::Acquire) || self.failure.is_some() {
                        reply.finish(Err(Error::Closed));
                    } else {
                        self.action_transaction(&id, sequence, mode, function, arguments, reply);
                    }
                }
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
                Event::Activated { result } => {
                    self.outstanding -= 1;
                    self.activated(result);
                }
                Event::Released { result } => {
                    self.outstanding -= 1;
                    self.released(result);
                }
                Event::SchedulerTick | Event::Wake => {}
            }
            if !stopped.load(Ordering::Acquire) {
                self.dispatch_jobs();
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
            Command::JobStatus { id, caller, reply } => reply.finish(self.scheduled.get(&id, &caller)),
            Command::WakeHandoff { reply } => reply.finish(Ok(self.scheduled.snapshot.wake.clone())),
            Command::JobControl { command, reply } => self.job_control(command, reply),
            Command::StartAction { id, call, reply } => self.start_action(id, call, reply),
            Command::ActionStatus { id, caller, reply } => reply.finish(self.actions.status(&id, &caller)),
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
            Command::Register { id, source, limits, reply } => {
                let result = if reply.cancellation.is_cancelled() {
                    Err(Error::Cancelled)
                } else if self.versions.len() >= MAX_DEPLOYMENTS {
                    Err(Error::Busy)
                } else {
                    self.js.register(id.clone(), source, limits).map_err(Error::from).map(|()| {
                        self.versions.insert(id, None);
                    })
                };
                reply.finish(result);
            }
            Command::Release { id, reply } => self.start_release(id, reply),
            Command::CheckDeployment { id, reply } => reply.finish(self.check_deployment(&id)),
            Command::Query { mut call, reply } => {
                if let Err(error) = self.normalize_call(&mut call) {
                    reply.finish(Err(error));
                    return;
                }
                self.query(&call, reply);
            }
            Command::Mutate { operation, mut call, reply } => match self.normalize_call(&mut call) {
                Ok(()) => self.mutate(operation, call, reply),
                Err(error) => reply.finish(Err(error)),
            },
            Command::Subscribe { mut calls, reply } => {
                match calls.iter_mut().try_for_each(|call| self.normalize_call(call)) {
                    Ok(()) => self.subscribe(calls, reply),
                    Err(error) => reply.finish(Err(error)),
                }
            }
        }
    }

    fn query(&mut self, call: &Call, reply: Request<Update>) {
        let result = self.evaluate(call, Mode::Query, self.view.clone(), &reply.cancellation);
        match result {
            Ok((execution, dependencies)) => {
                let independent = self.pending.iter().all(|pending| !dependencies.affected(&pending.changes));
                let update = Update {
                    revision: if independent { self.view.base.revision } else { self.view.revision },
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

    fn check_deployment(&self, id: &DeploymentId) -> Result<()> {
        if self.releasing.as_ref().is_some_and(|(releasing, _)| releasing == id) {
            return Err(Error::Busy);
        }
        self.versions.get(id).ok_or(Error::Unknown).map(|_| ())
    }

    fn normalize_call(&self, call: &mut Call) -> Result<()> {
        self.normalize_scoped_call(call, false)
    }

    fn normalize_scoped_call(&self, call: &mut Call, internal: bool) -> Result<()> {
        self.check_deployment(&call.deployment)?;
        if let Some(Some(deployment)) = self.versions.get(&call.deployment) {
            let function = deployment.functions.get(&call.function).ok_or(Error::Unknown)?;
            if !internal && function.visibility != Visibility::Public {
                return Err(Error::Unknown);
            }
            let mut arguments = serde_json::from_str(call.arguments.as_str())?;
            function.arguments.normalize_api(&mut arguments);
            if !function.arguments.accepts(&arguments) {
                return Err(Error::Contract);
            }
            call.arguments = arguments.into();
        }
        Ok(())
    }

    fn resolve(&self, call: &Call, mode: Mode) -> Result<Option<Function>> {
        if self.releasing.as_ref().is_some_and(|(id, _)| id == &call.deployment) {
            return Err(Error::Busy);
        }
        let version = self.versions.get(&call.deployment).ok_or(Error::Unknown)?;
        let Some(deployment) = version else {
            return Ok(None);
        };
        let function = deployment.functions.get(&call.function).ok_or(Error::Unknown)?;
        let kind = match mode {
            Mode::Query => FunctionKind::Query,
            Mode::Mutation => FunctionKind::Mutation,
        };
        if function.kind != kind {
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
        context: Option<(i64, u64, String)>,
    ) -> (Result<Execution>, Dependencies) {
        let function = match self.resolve(call, mode) {
            Ok(function) => function,
            Err(error) => return (Err(error), Dependencies::default()),
        };
        let trace = Rc::new(RefCell::new(Dependencies::default()));
        let operation = context.as_ref().map(|(_, _, operation)| operation.clone());
        let (timestamp, seed) = context.map_or_else(
            || {
                (view.base.timestamp, {
                    u64::from_be_bytes(Sha256::digest(call.function.as_bytes())[..8].try_into().expect("digest prefix"))
                })
            },
            |(timestamp, seed, _)| (timestamp, seed),
        );
        let host = Host {
            operation,
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
            .and_then(|mut execution| {
                for log in &execution.logs {
                    tracing::info!(target: "chunk_backend::console", deployment = call.deployment.as_str(), function = call.function, level = log.level, message = log.message);
                }
                let mut value = serde_json::from_str(&execution.value)?;
                if let Some(function) = &function {
                    function.result.normalize_api(&mut value);
                }
                validate_wire_value(&value).map_err(Error::Invalid)?;
                if function.as_ref().is_some_and(|f| !f.result.accepts(&value)) {
                    return Err(Error::Contract);
                }
                execution.value = serde_json::to_string(&value)?;
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
        self.actions.cancel();
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
