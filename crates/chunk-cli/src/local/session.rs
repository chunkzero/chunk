use std::{
    collections::BTreeSet,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

use chunk_proto::sync::v1::{Node, NodePhase, OperatorPlayer, SessionDemand};
use tokio::{
    sync::{mpsc, watch},
    task::{JoinError, JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

use super::{
    Command, Options, Settings, Staged,
    reload::{self, Change, Retirement},
    report::{self, Deployment, Reporter},
    services::{self, Observed, Shared, Version},
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
    version: Version,
    retirement: Option<Retirement>,
    nodes: Option<Vec<(String, Node)>>,
    players: Vec<(String, OperatorPlayer)>,
}

impl Live {
    fn new(version: Version) -> Self {
        Self { version, retirement: None, nodes: None, players: Vec::new() }
    }
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
    /// Backend releases in flight; each yields a deployment the backend still uses.
    retiring: JoinSet<Option<String>>,
    /// Stopped backend versions retried until no call, subscription or job still uses them.
    unreleased: Vec<String>,
    /// Retired releases whose JVMs have not all confirmed their exit. They keep their backend version and release
    /// directory until they have, however long that takes.
    stopping: Vec<Version>,
    /// Whether a release directory may have lost its last running version since the last prune.
    stale: bool,
}

impl<'a> Session<'a> {
    pub fn new(
        settings: &'a Settings,
        options: &'a Options,
        reporter: &'a Reporter,
        environment: String,
        shared: Shared,
        version: Version,
    ) -> Self {
        Self {
            settings,
            options,
            reporter,
            environment,
            shared,
            live: vec![Live::new(version)],
            pointer: None,
            retiring: JoinSet::new(),
            unreleased: Vec::new(),
            stopping: Vec::new(),
            stale: true,
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
        // Points `control.json` at control for `chunk players` and `chunk nodes`.
        let connection = self.shared.control_connection()?.clone();
        self.pointer = Some(chunk_service::Record::publish(&self.settings.state.join("control.json"), &connection)?);
        let (observed, _observing) = services::observe(connection);
        let reload = if self.options.no_watch { "r restarts" } else { "reloads on save · r restarts" };
        self.reporter.done(
            "Ready",
            format!(
                "connect to {} · {reload} · JVM logs in {}",
                self.settings.bind,
                self.settings.state.join("control").join("nodes").display()
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
                    if let Err(error) = self.observe(&observed) {
                        break Err(error);
                    }
                }
            }
            if build.is_none() {
                self.prune();
                if let Some(trigger) = queued.take() {
                    build = Some(self.rebuild(trigger, stop));
                }
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
                let root = self.options.project.canonicalize().unwrap_or_default();
                let path = path.strip_prefix(root).unwrap_or(&path).display().to_string();
                self.reporter.running("Reload", path);
            }
            Trigger::Restart => self.reporter.running("Reload", "forced restart"),
        }
        let (root, releases) = (self.options.project.clone(), self.settings.state.join("releases"));
        let (java, stop) = (self.options.java.clone(), stop.clone());
        let progress = self.reporter.build_progress();
        let task = tokio::spawn(async move {
            let started = Instant::now();
            let project = building::inspect(root.canonicalize()?, releases)?;
            let built = building::execute(&project, building::BuildMode::Dev, stop.clone(), progress).await?;
            let staged = super::stage(&project, built, java.as_deref(), &stop).await?;
            Ok((staged, started.elapsed()))
        });
        (task, forced)
    }

    async fn finish(&mut self, result: Result<io::Result<(Staged, Duration)>, JoinError>, forced: bool) {
        self.stale = true;
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
        let change =
            current.map_or(Change::Jvm, |index| reload::classify(&self.live[index].version.release, &staged.release));
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
        let resumed = self.live.iter().position(|live| live.version.release.id == id);
        let next = if let Some(index) = resumed {
            self.shared.activate(&self.live[index].version)?;
            self.shared.route(&self.live[index].version)?;
            self.live[index].retirement = None;
            index
        } else {
            let version = self.launch(staged).await?;
            self.live.push(Live::new(version));
            self.live.len() - 1
        };
        for (index, live) in self.live.iter_mut().enumerate().filter(|(index, _)| *index != next) {
            if Some(index) == current {
                live.retirement = Some(deadline.map_or_else(Retirement::pinned, Retirement::until));
            } else if let (Some(deadline), Some(retirement)) = (deadline, &mut live.retirement) {
                retirement.drain_by(deadline);
            }
        }
        let resumed = if resumed.is_some() { " resumed" } else { "" };
        Ok(format!("{}{resumed} · {summary}", short(&id)))
    }

    /// Makes `staged` current and routes new players to it, then stops every earlier release, disconnecting its
    /// players. The backend and control activate `staged` first, so a release either rejects leaves the running ones
    /// untouched.
    async fn restart(&mut self, staged: Staged) -> io::Result<String> {
        self.check_environment(&staged)?;
        let id = staged.release.id.clone();
        let version = self.launch(staged).await?;
        for live in std::mem::take(&mut self.live) {
            self.retire(live.version);
        }
        self.live.push(Live::new(version));
        Ok(format!("{} · restarted; previous sessions end", short(&id)))
    }

    fn check_environment(&self, staged: &Staged) -> io::Result<()> {
        if staged.control.deployment.environment != self.environment {
            return Err(io::Error::other("the [local] environment changed; restart chunk dev to use it"));
        }
        Ok(())
    }

    /// Activates the backend version and makes it control's current release, routing new players to it.
    async fn launch(&mut self, staged: Staged) -> io::Result<Version> {
        let deployment = staged.bundle.id.clone();
        self.shared.deploy(staged.bundle.clone()).await?;
        let version = Version::new(staged);
        if let Err(error) = self.shared.activate(&version).and_then(|()| self.shared.route(&version)) {
            self.release(deployment);
            return Err(error);
        }
        Ok(version)
    }

    /// Takes every release's latest nodes and players and stops retiring releases that are due.
    fn observe(&mut self, observed: &watch::Receiver<Option<Observed>>) -> io::Result<()> {
        if self.shared.failed() {
            return Err(io::Error::other("local backend or proxy stopped"));
        }
        if self.shared.control_failed() {
            return Err(io::Error::other("local control stopped"));
        }
        let observed = observed.borrow().clone();
        for live in &mut self.live {
            (live.nodes, live.players) = match &observed {
                Some(Observed { nodes, players }) => {
                    let mut nodes: Vec<_> = nodes
                        .iter()
                        .filter(|(_, node)| node.deployment == live.version.deployment)
                        .map(|(host, node)| (host.clone(), node.clone()))
                        .collect();
                    // Stopped nodes stay listed for their logs, below the running ones.
                    nodes.sort_by_key(|(_, node)| node.phase() == NodePhase::Stopped);
                    let players = players
                        .iter()
                        .filter(|(_, player)| nodes.iter().any(|(host, _)| *host == player.host))
                        .map(|(id, player)| (id.clone(), player.clone()))
                        .collect();
                    (Some(nodes), players)
                }
                None => (None, Vec::new()),
            };
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
            let due = live.retirement.as_mut().is_some_and(|retirement| retirement.due(live.nodes.as_deref(), now));
            if due {
                let live = self.live.remove(index);
                self.retire(live.version);
            } else {
                index += 1;
            }
        }
        self.confirm_stopped();
        self.reporter.deployments(
            self.live
                .iter()
                .rev()
                .map(|live| Deployment {
                    id: live.version.release.id.clone(),
                    state: live.retirement.as_ref().map_or_else(|| "current".into(), |r| r.describe(now)),
                    nodes: live.nodes.clone().unwrap_or_default(),
                    players: live.players.clone(),
                    destinations: live.version.destinations.clone(),
                })
                .collect(),
        );
        Ok(())
    }

    fn move_player(&self, deployment: &str, player: String, name: &str, demand: SessionDemand) {
        let reporter = self.reporter.clone();
        let target = format!("{name} → {}:{}", demand.session_type, demand.key);
        let live = self.live.iter().any(|live| live.version.release.id == deployment);
        let Some(connection) = self.shared.control_connection().ok().filter(|_| live).cloned() else {
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

    /// Retires `version` in control, which stops its JVMs at once.
    fn retire(&mut self, version: Version) {
        self.stopping.push(version);
        self.confirm_stopped();
    }

    /// Asks control to stop each retired release's JVMs, and releases the backend version of each release whose JVMs
    /// have all confirmed their exit. Only that confirmation ends a release's stop.
    fn confirm_stopped(&mut self) {
        let Ok(control) = self.shared.control() else { return };
        for version in std::mem::take(&mut self.stopping) {
            match control.retire_release(&version.deployment) {
                Ok(true) => {
                    self.reporter.done("Retire", format!("{} stopped", short(&version.release.id)));
                    self.stale = true;
                    self.release(version.deployment);
                }
                Ok(false) => self.stopping.push(version),
                Err(error) => {
                    tracing::warn!(%error, deployment = version.deployment, "retired release not yet stopping");
                    self.stopping.push(version);
                }
            }
        }
    }

    fn release(&mut self, deployment: String) {
        if let Some(backend) = self.shared.backend() {
            self.retiring.spawn(async move { (!release(&backend, &deployment).await).then_some(deployment) });
        }
    }

    /// Deletes release directories no running or stopping version uses. Called only between builds, since a build may
    /// still read a release that is no longer live.
    fn prune(&mut self) {
        if !self.stale {
            return;
        }
        self.stale = false;
        let versions = self.live.iter().map(|live| &live.version).chain(&self.stopping);
        let live: BTreeSet<_> = versions.map(|version| version.release.id.as_str()).collect();
        if let Err(error) = crate::cleaning::prune(&self.settings.state.join("releases"), &live) {
            tracing::warn!(%error, "unused releases not pruned");
        }
    }

    async fn stop(mut self) -> io::Result<()> {
        let mut result = self.shared.stop_proxy().await;
        let mut unreleased = std::mem::take(&mut self.unreleased);
        while let Some(finished) = self.retiring.join_next().await {
            unreleased.extend(finished.ok().flatten());
        }
        // Stopping control returns only once every JVM left has confirmed its exit, which frees their backend versions.
        if let Err(error) = self.shared.stop_control(self.reporter).await {
            result = Err(error);
        }
        let stopping = self.stopping.drain(..);
        let versions = self.live.drain(..).map(|live| live.version).chain(stopping);
        unreleased.extend(versions.map(|version| version.deployment));
        if let Some(backend) = self.shared.backend() {
            for deployment in unreleased {
                release_before_exit(&backend, &deployment).await;
            }
        }
        self.pointer = None;
        let stopped = self.shared.stop(self.reporter).await;
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
