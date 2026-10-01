use std::{
    collections::BTreeSet,
    sync::{Arc, mpsc},
    thread::JoinHandle,
};

use chunk_contract::{Deployment, Function};
use chunk_js::{Cancellation, DeploymentId, Engine, Limits, Mode};

use super::watches::Job;
use crate::{
    Error, Result,
    evaluate::{Target, evaluate},
    reads::{Change, Dependencies, View},
    service::{Call, Event, Request, Update},
    timing::{Phase, Timer},
};

/// Bundle source that read engines load on first use.
pub(crate) struct Source {
    pub code: String,
    pub limits: Limits,
    /// What `ctx.env` reads.
    pub env: chunk_js::Json,
    /// Redacts what queries log.
    pub secrets: crate::effects::SecretSlot,
}

pub(crate) enum Ticket {
    /// `overlay` holds the staged writes the view included, to pick the reply's revision.
    Query {
        reply: Request<Update>,
        epoch: u64,
        overlay: Vec<Arc<[Change]>>,
    },
    Watch(Job),
}

/// One query evaluation handed to a read engine and back.
pub(crate) struct Read {
    pub ticket: Ticket,
    pub call: Call,
    pub function: Option<Function>,
    pub contract: Option<Arc<Deployment>>,
    pub source: Arc<Source>,
    pub view: Arc<View>,
    pub cancellation: Cancellation,
}

pub(crate) struct Evaluated {
    pub worker: usize,
    pub read: Read,
    pub result: Result<String>,
    pub reads: Dependencies,
}

enum Work {
    Read(Box<Read>),
    Release(DeploymentId),
}

struct Worker {
    jobs: Option<mpsc::Sender<Work>>,
    thread: Option<JoinHandle<()>>,
    /// Deployment of the read in progress.
    busy: Option<DeploymentId>,
}

/// Engines for queries and subscription reevaluations. Each worker runs one read
/// at a time; the engine thread queues the rest and keeps mutations to itself.
pub(super) struct Readers {
    workers: Vec<Worker>,
    stop: Cancellation,
}

impl Readers {
    pub fn new(count: usize, events: &tokio::sync::mpsc::Sender<Event>) -> Result<Self> {
        let workers = (0..count)
            .map(|index| {
                let (jobs, incoming) = mpsc::channel();
                let events = events.clone();
                let thread = std::thread::Builder::new()
                    .name(format!("chunk-read-{index}"))
                    .spawn(move || work(index, &incoming, &events))?;
                Ok(Worker { jobs: Some(jobs), thread: Some(thread), busy: None })
            })
            .collect::<Result<_>>()?;
        Ok(Self { workers, stop: Cancellation::default() })
    }

    /// Cancels subscription reevaluations when the backend stops.
    pub fn stopping(&self) -> Cancellation {
        self.stop.clone()
    }

    pub fn alive(&self) -> bool {
        self.workers.iter().any(|worker| worker.jobs.is_some())
    }

    pub fn idle(&self) -> bool {
        self.workers.iter().any(|worker| worker.busy.is_none() && worker.jobs.is_some())
    }

    pub fn references(&self, deployment: &DeploymentId) -> bool {
        self.workers.iter().any(|worker| worker.busy.as_ref() == Some(deployment))
    }

    /// Hands the read to an idle worker, or returns it if none can take it.
    pub fn send(&mut self, read: Read) -> std::result::Result<(), Box<Read>> {
        let Some(worker) = self.workers.iter_mut().find(|worker| worker.busy.is_none() && worker.jobs.is_some()) else {
            return Err(Box::new(read));
        };
        let deployment = read.call.deployment.clone();
        let jobs = worker.jobs.as_ref().expect("live worker");
        match jobs.send(Work::Read(Box::new(read))) {
            Ok(()) => {
                worker.busy = Some(deployment);
                Ok(())
            }
            Err(mpsc::SendError(work)) => {
                worker.jobs = None;
                let Work::Read(read) = work else { unreachable!("sent a read") };
                Err(read)
            }
        }
    }

    pub fn done(&mut self, worker: usize) {
        self.workers[worker].busy = None;
    }

    pub fn release(&self, deployment: &DeploymentId) {
        for worker in &self.workers {
            if let Some(jobs) = &worker.jobs {
                let _ = jobs.send(Work::Release(deployment.clone()));
            }
        }
    }
}

impl Drop for Readers {
    fn drop(&mut self) {
        self.stop.cancel();
        for worker in &mut self.workers {
            worker.jobs.take();
        }
        for worker in &mut self.workers {
            if let Some(thread) = worker.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

fn work(index: usize, incoming: &mpsc::Receiver<Work>, events: &tokio::sync::mpsc::Sender<Event>) {
    let mut engine = Engine::new().map_err(Error::from);
    let mut loaded = BTreeSet::new();
    while let Ok(work) = incoming.recv() {
        let read = match work {
            Work::Release(deployment) => {
                if loaded.remove(&deployment)
                    && let Ok(engine) = &mut engine
                {
                    engine.release(&deployment);
                }
                continue;
            }
            Work::Read(read) => *read,
        };
        let (result, reads) = match &mut engine {
            Ok(engine) => run(engine, &mut loaded, &read),
            Err(error) => (Err(error.clone()), Dependencies::default()),
        };
        let evaluated = Evaluated { worker: index, read, result, reads };
        if events.blocking_send(Event::Evaluated(Box::new(evaluated))).is_err() {
            break;
        }
    }
}

fn run(engine: &mut Engine, loaded: &mut BTreeSet<DeploymentId>, read: &Read) -> (Result<String>, Dependencies) {
    let deployment = &read.call.deployment;
    if !loaded.contains(deployment) {
        let source = &read.source;
        if let Err(error) =
            engine.register_with_env(deployment.clone(), source.code.clone(), source.limits, source.env.clone())
        {
            return (Err(error.into()), Dependencies::default());
        }
        loaded.insert(deployment.clone());
    }
    let timer = Timer::start();
    let target = Target {
        call: &read.call,
        function: read.function.as_ref(),
        contract: read.contract.clone(),
        secrets: &read.source.secrets,
    };
    let (result, reads) = evaluate(engine, target, Mode::Query, read.view.clone(), &read.cancellation, None);
    timer.stop(match read.ticket {
        Ticket::Query { .. } => Phase::Query,
        Ticket::Watch(_) => Phase::Reevaluate,
    });
    (result.map(|execution| execution.value), reads)
}
