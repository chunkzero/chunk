use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

use chunk_contract::{Deployment, Function, FunctionKind, Visibility};
use chunk_js::{Cancellation, DeploymentId, Engine, Execution, Limits, Mode};
use chunk_store::{Operation, Revision, Storage, Write};
use tokio::sync::mpsc;

use crate::{
    Error, Result,
    commit::{Committer, Job},
    evaluate::{Target, evaluate},
    limits::EngineQueue,
    reads::{Change, Dependencies, View},
    service::{Call, Command, Event, Request, Update},
    timing::{Phase, Timer},
};

mod actions;
mod commands;
mod deployments;
mod index;
mod jobs;
mod pipeline;
mod queries;
mod readers;
mod subscriptions;
mod watches;

pub(crate) use readers::Evaluated;

const MAX_DEPLOYMENTS: usize = 16;

struct Mutation {
    operation: Operation,
    context: Option<chunk_store::RetryContext>,
    call: Call,
    waiters: Vec<Request<Update>>,
    admitted: Instant,
}

struct Pending {
    operation: String,
    revision: Revision,
    writes: Vec<Write>,
    changes: Arc<[Change]>,
    bytes: usize,
    staged: Timer,
}

pub(crate) struct Actor {
    actions: actions::Actions,
    scheduled: jobs::Scheduled,
    /// Bounds the idle wait so due jobs dispatch without another event arriving.
    timer: tokio::runtime::Runtime,
    recovering: bool,
    watches: watches::Watches,
    js: Engine,
    readers: readers::Readers,
    sources: BTreeMap<DeploymentId, Arc<readers::Source>>,
    reads: VecDeque<queries::Waiting>,
    /// Whether a subscription reevaluation goes before the next queued query.
    rerun_turn: bool,
    /// Increments when staged writes roll back, so queries that read them run again.
    epoch: u64,
    versions: BTreeMap<DeploymentId, Option<Arc<Deployment>>>,
    deploying: Option<(Arc<Deployment>, Request<()>)>,
    releasing: Option<(DeploymentId, Request<bool>)>,
    view: Arc<View>,
    catalogs: BTreeMap<(DeploymentId, String), commands::CachedCatalog>,
    committer: Committer,
    outstanding: usize,
    mutations: BTreeMap<String, Mutation>,
    pending: VecDeque<Pending>,
    pending_bytes: usize,
    deferred: VecDeque<(Update, Request<Update>)>,
    queue: Arc<EngineQueue>,
    /// Request memory, charged for query replies retained until durable.
    memory: Arc<tokio::sync::Semaphore>,
    /// System commits acknowledged so far.
    system: u64,
    failure: Option<Error>,
}

impl Actor {
    pub fn new(
        store: Box<dyn Storage>,
        events: mpsc::Sender<Event>,
        effects: crate::ActionEffects,
        action_bytes: usize,
        readers: usize,
        queue: Arc<EngineQueue>,
        memory: Arc<tokio::sync::Semaphore>,
    ) -> Result<Self> {
        let (committer, snapshot, deployments, scheduled) = Committer::new(store, events.clone())?;
        let mut js = Engine::new()?;
        let mut versions = BTreeMap::new();
        let mut sources = BTreeMap::new();
        for deployment in deployments {
            deployment.validate().map_err(Error::Invalid)?;
            Self::schema_ready(&deployment, snapshot.schema())?;
            let id = DeploymentId::new(&deployment.id)?;
            js.register(id.clone(), deployment.source.clone(), Limits::default())?;
            let source = readers::Source { code: deployment.source.clone(), limits: Limits::default() };
            sources.insert(id.clone(), Arc::new(source));
            versions.insert(id, Some(Arc::new(deployment)));
        }
        let readers = readers::Readers::new(readers, &events)?;
        Ok(Self {
            scheduled: jobs::Scheduled::new(scheduled),
            timer: tokio::runtime::Builder::new_current_thread().enable_time().build()?,
            actions: actions::Actions::new(events, uuid::Uuid::new_v4().to_string(), effects, action_bytes),
            recovering: false,
            watches: watches::Watches::new(snapshot.revision),
            js,
            readers,
            sources,
            reads: VecDeque::new(),
            rerun_turn: false,
            epoch: 0,
            versions,
            deploying: None,
            releasing: None,
            view: Arc::new(View::new(snapshot)),
            catalogs: BTreeMap::new(),
            committer,
            outstanding: 0,
            mutations: BTreeMap::new(),
            pending: VecDeque::new(),
            pending_bytes: 0,
            deferred: VecDeque::new(),
            queue,
            memory,
            system: 0,
            failure: None,
        })
    }

    pub fn run(mut self, mut incoming: mpsc::Receiver<Event>, stopped: &AtomicBool) {
        loop {
            if stopped.load(Ordering::Acquire) && self.outstanding == 0 {
                break;
            }
            let event = if let Some(wait) = self.scheduled.next_due() {
                self.timer
                    .block_on(async { tokio::time::timeout(wait, incoming.recv()).await })
                    .unwrap_or(Some(Event::Wake))
            } else {
                incoming.blocking_recv()
            };
            let Some(event) = event else {
                break;
            };
            match event {
                Event::Scheduled { command, result } => {
                    self.outstanding -= 1;
                    self.scheduled(command, result);
                    self.recovering &= self.outstanding != 0;
                }
                Event::ActionPlatform { id, sequence, request, reply } => {
                    if stopped.load(Ordering::Acquire) || self.failure.is_some() {
                        reply.finish(Err(Error::Closed));
                    } else {
                        self.action_platform(&id, sequence, &request, reply);
                    }
                }
                Event::ActionFinished { id, result } => self.finish_action(&id, result),
                Event::ActionTransaction { id, sequence, mode, function, arguments, reply } => {
                    if stopped.load(Ordering::Acquire) || self.failure.is_some() {
                        reply.finish(Err(Error::Closed));
                    } else {
                        self.action_transaction(&id, sequence, mode, function, arguments, reply);
                    }
                }
                Event::Request { command, admitted } => {
                    self.queue.dequeued(admitted);
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
                Event::Evaluated(evaluated) => self.evaluated(*evaluated),
                Event::System { count, revision, snapshot } => self.system_committed(count, revision, snapshot),
                Event::Failed => {
                    if self.failure.is_none() {
                        self.fail(&Error::CommitFailed);
                    }
                }
                Event::Wake => {}
            }
            if !stopped.load(Ordering::Acquire) {
                self.dispatch_actions();
                self.dispatch_jobs();
            }
            self.dispatch();
        }
        incoming.close();
        while let Ok(event) = incoming.try_recv() {
            if let Event::Request { command, admitted } = event {
                self.queue.dequeued(admitted);
                command.reject(Error::Closed);
            }
        }
        self.fail(&Error::Closed);
        // Close the reply path before joining the committer, including early shutdown.
        drop(incoming);
    }

    fn request(&mut self, command: Command) {
        match command {
            Command::Catalog { id, scope, reply } => {
                let result = self.command_catalog(&id, &scope, &reply.cancellation);
                reply.finish(result);
            }
            Command::Suggest { id, request, reply } => {
                let result = self.command_suggest(&id, request, &reply.cancellation);
                reply.finish(result);
            }
            Command::Prepare { id, scope, command, input, reply } => {
                let result = self.prepare_command(id, scope, command, input, &reply.cancellation);
                reply.finish(result);
            }
            Command::DomainManifest { id, reply } => reply.finish(self.check_deployment(&id).and_then(|()| {
                self.versions
                    .get(&id)
                    .and_then(Option::as_ref)
                    .map(|deployment| deployment.contracts.domains.clone())
                    .ok_or(Error::Contract)
            })),
            Command::Functions { id, reply } => reply.finish(self.check_deployment(&id).map(|()| {
                let deployment = self.versions.get(&id).and_then(Option::as_ref);
                deployment
                    .iter()
                    .flat_map(|deployment| &deployment.functions)
                    .filter(|(_, function)| function.visibility == Visibility::Public)
                    .map(|(name, function)| (name.clone(), function.kind))
                    .collect()
            })),
            Command::PrepareAction { reply } => reply.finish(self.actions.prepare()),
            Command::ActionIdentity { id, reply } => reply.finish(self.actions.identity(&id)),
            Command::StartAction { purpose, id, call, retain, reply } => {
                self.start_action(id, call, purpose, retain, reply);
            }
            Command::JobStatus { id, caller, reply } => reply.finish(self.scheduled.get(&id, &caller)),
            Command::WakeHandoff { reply } => reply.finish(Ok(self.scheduled.snapshot.wake.clone())),
            Command::JobControl { command, reply } => self.job_control(command, reply),
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
                    self.js.register(id.clone(), source.clone(), limits).map_err(Error::from).map(|()| {
                        self.sources.insert(id.clone(), Arc::new(readers::Source { code: source, limits }));
                        self.versions.insert(id, None);
                    })
                };
                reply.finish(result);
            }
            Command::Release { id, reply } => self.start_release(id, reply),
            Command::CheckDeployment { id, reply } => reply.finish(self.check_deployment(&id)),
            Command::Deployments { reply } => reply.finish(Ok(self.versions.keys().cloned().collect())),
            Command::Query { mut call, reply } => match self.normalize_call(&mut call) {
                Ok(()) => self.query(call, reply),
                Err(error) => reply.finish(Err(error)),
            },
            Command::Mutate { operation, mut call, reply } => {
                reply.queued.stop(Phase::Queue);
                match self.normalize_call(&mut call) {
                    Ok(()) => self.mutate(operation, call, reply),
                    Err(error) => reply.finish(Err(error)),
                }
            }
            Command::Subscribe { mut calls, reply } => {
                match calls.iter_mut().try_for_each(|call| self.normalize_call(call)) {
                    Ok(()) => self.subscribe(calls, reply),
                    Err(error) => reply.finish(Err(error)),
                }
            }
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
        view: Arc<View>,
        cancellation: &Cancellation,
    ) -> Result<(Execution, Dependencies)> {
        let (execution, dependencies) = self.evaluate_traced(call, mode, view, cancellation, None);
        execution.map(|execution| (execution, dependencies))
    }

    fn evaluate_traced(
        &mut self,
        call: &Call,
        mode: Mode,
        view: Arc<View>,
        cancellation: &Cancellation,
        context: Option<(i64, u64, String)>,
    ) -> (Result<Execution>, Dependencies) {
        let function = match self.resolve(call, mode) {
            Ok(function) => function,
            Err(error) => return (Err(error), Dependencies::default()),
        };
        let contract = self.versions.get(&call.deployment).cloned().flatten();
        let target = Target { call, function: function.as_ref(), contract };
        evaluate(&mut self.js, target, mode, view, cancellation, context)
    }

    pub fn lane(&self) -> Arc<crate::system::Lane> {
        self.committer.lane()
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
        self.epoch += 1;
        self.view = Arc::new(View::new(self.view.base.clone()));
    }

    fn fail(&mut self, error: &Error) {
        self.actions.cancel();
        self.actions.refuse_waiting(error);
        self.failure = Some(error.clone());
        self.reset_pending(error);
        for waiting in self.reads.drain(..) {
            waiting.fail(error);
        }
        self.watches.fail(error);
    }
}
