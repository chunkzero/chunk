use std::{
    io,
    net::SocketAddr,
    path::PathBuf,
    time::{Duration, Instant},
};

use chunk_proto::v1::{NodeStatus, PlayerStatus, SessionDemand};
use tokio::{
    sync::mpsc,
    task::{JoinError, JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use super::{
    Command, Options, Settings, Staged,
    reload::{self, Change, Retirement},
    report::{self, Deployment, Reporter},
    services::{self, Generation, Shared},
    short,
};
use crate::building;

/// Source changes closer together than this share one rebuild.
const QUIET: Duration = Duration::from_millis(300);

type Build = JoinHandle<io::Result<(Staged, Duration)>>;
type Watched = (reload::Watcher, mpsc::UnboundedReceiver<PathBuf>);

enum Trigger {
    Change(PathBuf),
    Restart,
}

/// A running release; the current one has no retirement.
struct Live {
    generation: Generation,
    retirement: Option<Retirement>,
    nodes: Option<Vec<NodeStatus>>,
    players: Vec<PlayerStatus>,
}

/// Owns the shared services and every running release, rebuilding and replacing releases on request.
pub(super) struct Session<'a> {
    settings: &'a Settings,
    options: &'a Options,
    reporter: &'a Reporter,
    environment: String,
    shared: Shared,
    live: Vec<Live>,
    pointer: Option<chunk_service::Record>,
    /// Stops and backend releases in flight; each yields a deployment the backend still uses.
    retiring: JoinSet<Option<String>>,
    /// Stopped backend versions retried until no call, subscription or job still uses them.
    unreleased: Vec<String>,
}

impl<'a> Session<'a> {
    pub fn new(
        settings: &'a Settings,
        options: &'a Options,
        reporter: &'a Reporter,
        environment: String,
        shared: Shared,
        generation: Generation,
    ) -> Self {
        Self {
            settings,
            options,
            reporter,
            environment,
            shared,
            live: vec![Live { generation, retirement: None, nodes: None, players: Vec::new() }],
            pointer: None,
            retiring: JoinSet::new(),
            unreleased: Vec::new(),
        }
    }

    pub async fn run(
        mut self,
        watched: Option<Watched>,
        mut commands: mpsc::UnboundedReceiver<Command>,
        stop: CancellationToken,
    ) -> io::Result<()> {
        let (mut sources, mut changes) = match watched {
            Some((sources, changes)) => (Some(sources), changes),
            None => (None, mpsc::unbounded_channel().1),
        };
        let result = self.serve(sources.as_mut(), &mut changes, &mut commands, &stop).await;
        let reporter = self.reporter;
        reporter.running("Stop", "");
        let started = Instant::now();
        let stopped = self.stop().await;
        reporter.done("Stop", report::seconds(started.elapsed()));
        result.and(stopped)
    }

    async fn serve(
        &mut self,
        mut watcher: Option<&mut reload::Watcher>,
        changes: &mut mpsc::UnboundedReceiver<PathBuf>,
        commands: &mut mpsc::UnboundedReceiver<Command>,
        stop: &CancellationToken,
    ) -> io::Result<()> {
        self.publish()?;
        let deployment = &self.live[0].generation.deployment;
        let reload = if self.options.no_watch { "r restarts" } else { "reloads on save · r restarts" };
        self.reporter.done(
            "Ready",
            format!(
                "connect to {} · {reload} · JVM logs in {}",
                self.settings.bind,
                self.settings.state.join("control").join(deployment).join("nodes").display()
            ),
        );
        let mut build: Option<(Build, bool)> = None;
        let mut queued = None;
        let mut quiet: Option<(tokio::time::Instant, PathBuf)> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let result = loop {
            let settle = quiet.as_ref().map(|(deadline, _)| *deadline);
            tokio::select! {
                () = stop.cancelled() => break Ok(()),
                Some(path) = changes.recv() => {
                    if let Some(watcher) = watcher.as_mut() {
                        watcher.cover(&path);
                    }
                    quiet = Some((tokio::time::Instant::now() + QUIET, path));
                }
                () = tokio::time::sleep_until(settle.unwrap_or_else(tokio::time::Instant::now)), if settle.is_some() => {
                    if let Some((_, path)) = quiet.take()
                        && !matches!(queued, Some(Trigger::Restart))
                    {
                        queued = Some(Trigger::Change(path));
                    }
                }
                Some(command) = commands.recv() => match command {
                    Command::Restart => queued = Some(Trigger::Restart),
                    Command::MovePlayer { deployment, player, name, demand } => {
                        self.move_player(&deployment, player, &name, demand);
                    }
                },
                (result, forced) = finished(&mut build) => self.finish(result, forced).await,
                _ = tick.tick() => {
                    if let Err(error) = self.observe().await {
                        break Err(error);
                    }
                }
            }
            if build.is_none()
                && let Some(trigger) = queued.take()
            {
                build = Some(self.rebuild(trigger, stop));
            }
        };
        if let Some((task, _)) = build {
            task.abort();
            let _ = task.await;
        }
        result
    }

    fn rebuild(&self, trigger: Trigger, stop: &CancellationToken) -> (Build, bool) {
        let forced = matches!(trigger, Trigger::Restart);
        match trigger {
            Trigger::Change(path) => {
                let root = self.options.build.project.canonicalize().unwrap_or_default();
                let path = path.strip_prefix(root).unwrap_or(&path).display().to_string();
                self.reporter.running("Reload", path);
            }
            Trigger::Restart => self.reporter.running("Reload", "forced restart"),
        }
        let (options, java, stop) = (self.options.build.clone(), self.options.java.clone(), stop.clone());
        let progress = self.reporter.build_progress();
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let project = building::prepare(&options)?;
            let built = building::execute(&project, stop.clone(), progress).await?;
            let staged = super::stage(&project, built, java.as_deref(), &stop).await?;
            Ok((staged, started.elapsed()))
        });
        (task, forced)
    }

    async fn finish(&mut self, result: Result<io::Result<(Staged, Duration)>, JoinError>, forced: bool) {
        let outcome = match result.map_err(io::Error::other).and_then(|built| built) {
            Ok((staged, elapsed)) => {
                let release = short(&staged.release.id).to_owned();
                let deployed = if forced { self.restart(staged).await } else { self.deploy(staged).await };
                deployed.map(|summary| {
                    self.reporter.done("Build", format!("{} · release {release}", report::seconds(elapsed)));
                    format!("{} · {summary}", report::seconds(elapsed))
                })
            }
            Err(error) => Err(error),
        };
        match outcome {
            Ok(summary) => self.reporter.done("Reload", summary),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if forced => self.reporter.failed("Reload", error),
            Err(error) => self.reporter.failed("Reload", format!("{error}\nthe previous release keeps serving")),
        }
    }

    /// Starts `staged` beside the current release and routes new players to it.
    async fn deploy(&mut self, staged: Staged) -> io::Result<String> {
        self.check_environment(&staged)?;
        let current = self.live.iter().position(|live| live.retirement.is_none());
        let change = current
            .map_or(Change::Jvm, |index| reload::classify(&self.live[index].generation.release, &staged.release));
        let drain = Duration::from_secs(self.options.drain_seconds);
        let (deadline, summary) = match change {
            Change::Unchanged => return Ok("no changes".into()),
            Change::Backend => (None, "backend only; existing sessions stay pinned".to_owned()),
            Change::Jvm => (
                Some(Instant::now() + drain),
                format!("JVM change; earlier releases drain within {}s", drain.as_secs()),
            ),
        };
        let id = staged.release.id.clone();
        let resumed = self.live.iter().position(|live| live.generation.release.id == id);
        let next = if let Some(index) = resumed {
            self.shared.route(&self.live[index].generation)?;
            self.live[index].retirement = None;
            index
        } else {
            let generation = self.launch(staged).await?;
            self.live.push(Live { generation, retirement: None, nodes: None, players: Vec::new() });
            self.live.len() - 1
        };
        for (index, live) in self.live.iter_mut().enumerate().filter(|(index, _)| *index != next) {
            if Some(index) == current {
                live.retirement = Some(deadline.map_or_else(Retirement::pinned, Retirement::until));
            } else if let (Some(deadline), Some(retirement)) = (deadline, &mut live.retirement) {
                retirement.drain_by(deadline);
            }
        }
        self.publish()?;
        let resumed = if resumed.is_some() { " resumed" } else { "" };
        Ok(format!("{}{resumed} · {summary}", short(&id)))
    }

    /// Stops every running release, disconnecting its players, then starts `staged`.
    async fn restart(&mut self, staged: Staged) -> io::Result<String> {
        self.check_environment(&staged)?;
        let id = staged.release.id.clone();
        let mut stopped = Vec::new();
        for live in self.live.drain(..) {
            stopped.push(live.generation.deployment.clone());
            if let Err(error) = live.generation.stop().await {
                tracing::error!(%error, "control shutdown failed");
            }
        }
        self.pointer = None;
        for deployment in stopped {
            self.release(deployment);
        }
        let generation = self.launch(staged).await?;
        self.live.push(Live { generation, retirement: None, nodes: None, players: Vec::new() });
        self.publish()?;
        Ok(format!("{} · restarted; previous sessions ended", short(&id)))
    }

    fn check_environment(&self, staged: &Staged) -> io::Result<()> {
        if staged.control.deployment.environment != self.environment {
            return Err(io::Error::other("the [local] environment changed; restart chunk dev to use it"));
        }
        Ok(())
    }

    /// Activates the backend version and starts its control, routing new players to it.
    async fn launch(&mut self, staged: Staged) -> io::Result<Generation> {
        let deployment = staged.bundle.id.clone();
        self.shared.deploy(staged.bundle.clone()).await?;
        let bind = SocketAddr::new(self.settings.control_bind.ip(), 0);
        let started = Generation::start(self.settings, &self.shared, staged, bind).await;
        let routed = match started {
            Ok(generation) => match self.shared.route(&generation) {
                Ok(()) => return Ok(generation),
                Err(error) => {
                    if let Err(error) = generation.stop().await {
                        tracing::error!(%error, "control shutdown failed");
                    }
                    error
                }
            },
            Err(error) => error,
        };
        self.release(deployment);
        Err(routed)
    }

    /// Polls every release's nodes and stops retiring releases that are due.
    async fn observe(&mut self) -> io::Result<()> {
        if self.shared.failed() {
            return Err(io::Error::other("local backend or proxy stopped"));
        }
        for live in &mut self.live {
            let observed = match live.generation.connection() {
                Some(connection) => services::observe(connection).await.ok(),
                None => None,
            };
            (live.nodes, live.players) = observed.map_or((None, Vec::new()), |(nodes, players)| (Some(nodes), players));
        }
        if self.live.iter().any(|live| live.retirement.is_none() && live.generation.failed()) {
            return Err(io::Error::other("local control stopped"));
        }
        while let Some(finished) = self.retiring.try_join_next() {
            self.unreleased.extend(finished.ok().flatten());
        }
        for deployment in std::mem::take(&mut self.unreleased) {
            self.release(deployment);
        }
        let now = Instant::now();
        let mut index = 0;
        while index < self.live.len() {
            let live = &mut self.live[index];
            let due = live.generation.failed()
                || live.retirement.as_mut().is_some_and(|retirement| retirement.due(live.nodes.as_deref(), now));
            if due {
                let live = self.live.remove(index);
                self.retire(live.generation);
            } else {
                index += 1;
            }
        }
        self.reporter.deployments(
            self.live
                .iter()
                .rev()
                .map(|live| Deployment {
                    id: live.generation.release.id.clone(),
                    state: live.retirement.as_ref().map_or_else(|| "current".into(), |r| r.describe(now)),
                    nodes: live.nodes.clone().unwrap_or_default(),
                    players: live.players.clone(),
                    session_types: live.generation.session_types.clone(),
                })
                .collect(),
        );
        Ok(())
    }

    fn move_player(&self, deployment: &str, player: String, name: &str, demand: SessionDemand) {
        let reporter = self.reporter.clone();
        let target = format!("{name} → {}:{}", demand.session_type, demand.key);
        let live = self.live.iter().find(|live| live.generation.release.id == deployment);
        let Some(connection) = live.and_then(|live| live.generation.connection()).cloned() else {
            reporter.failed("Move", format!("{target}: release {} is no longer running", short(deployment)));
            return;
        };
        reporter.running("Move", &target);
        tokio::spawn(async move {
            match services::move_player(&connection, player, demand).await {
                Ok(()) => reporter.done("Move", format!("{target} queued")),
                Err(error) => reporter.failed("Move", format!("{target}: {error}")),
            }
        });
    }

    fn retire(&mut self, generation: Generation) {
        let (backend, reporter) = (self.shared.backend(), self.reporter.clone());
        self.retiring.spawn(async move {
            let (release_id, deployment) = (generation.release.id.clone(), generation.deployment.clone());
            match generation.stop().await {
                Ok(()) => reporter.done("Retire", format!("{} stopped", short(&release_id))),
                Err(error) => reporter.failed("Retire", format!("{}: {error}", short(&release_id))),
            }
            let backend = backend?;
            (!release(&backend, &deployment).await).then_some(deployment)
        });
    }

    fn release(&mut self, deployment: String) {
        if let Some(backend) = self.shared.backend() {
            self.retiring.spawn(async move { (!release(&backend, &deployment).await).then_some(deployment) });
        }
    }

    /// Points `control.json` at the current release for `chunk players` and `chunk nodes`.
    fn publish(&mut self) -> io::Result<()> {
        let current = self.live.iter().find(|live| live.retirement.is_none());
        self.pointer = match current.and_then(|live| live.generation.connection()) {
            Some(connection) => {
                Some(chunk_service::Record::publish(&self.settings.state.join("control.json"), connection)?)
            }
            None => None,
        };
        Ok(())
    }

    async fn stop(mut self) -> io::Result<()> {
        let mut result = self.shared.stop_proxy().await;
        for live in std::mem::take(&mut self.live) {
            let deployment = live.generation.deployment.clone();
            if let Err(error) = live.generation.stop().await {
                result = Err(error);
            }
            self.release(deployment);
        }
        let mut unreleased = std::mem::take(&mut self.unreleased);
        while let Some(finished) = self.retiring.join_next().await {
            unreleased.extend(finished.ok().flatten());
        }
        if let Some(backend) = self.shared.backend() {
            for deployment in unreleased {
                release_before_exit(&backend, &deployment).await;
            }
        }
        self.pointer = None;
        let stopped = self.shared.stop().await;
        result.and(stopped)
    }
}

async fn finished(build: &mut Option<(Build, bool)>) -> (Result<io::Result<(Staged, Duration)>, JoinError>, bool) {
    let Some((task, forced)) = build else { return std::future::pending().await };
    let result = task.await;
    let forced = *forced;
    *build = None;
    (result, forced)
}

/// Tries once to release a stopped backend version; false while a call, subscription or job still uses it.
async fn release(backend: &chunk_backend::Backend, id: &str) -> bool {
    let Ok(deployment) = chunk_backend::DeploymentId::new(id) else { return true };
    match backend.release(deployment).await {
        Err(chunk_backend::Error::Busy) => false,
        Err(error) => {
            tracing::warn!(%error, deployment = id, "backend version not released");
            true
        }
        Ok(_) => true,
    }
}

/// Bounded release retries while the session shuts down.
async fn release_before_exit(backend: &chunk_backend::Backend, id: &str) {
    for _ in 0..10 {
        if release(backend, id).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    tracing::warn!(deployment = id, "backend version still in use; not released");
}
