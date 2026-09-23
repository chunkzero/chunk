use std::{fmt::Display, time::Duration};

use chunk_proto::v1::{NodeStatus, PlayerStatus, SessionDemand};
use tokio::sync::mpsc;

use crate::building::progress;

/// Progress and diagnostics from a dev session, rendered as plain lines or by the TUI.
pub(crate) enum Event {
    Step { name: &'static str, state: Step },
    Log { source: Source, line: String },
    Deployments(Vec<Deployment>),
}

pub(crate) enum Step {
    Pending(String),
    Running(String),
    Done(String),
    Failed(String),
}

/// One running release and the nodes and players its control authority reports.
pub(crate) struct Deployment {
    pub id: String,
    pub state: String,
    pub nodes: Vec<NodeStatus>,
    pub players: Vec<PlayerStatus>,
    pub destinations: Vec<Destination>,
}

/// A declared destination players can be moved to; only these carry their session's configuration.
#[derive(Clone)]
pub(crate) struct Destination {
    pub name: String,
    pub demand: SessionDemand,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Dev,
    Build,
    Proxy,
    Control,
    Backend,
    Jvm,
}

impl Source {
    pub const ALL: [Self; 6] = [Self::Dev, Self::Build, Self::Proxy, Self::Control, Self::Backend, Self::Jvm];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Build => "build",
            Self::Proxy => "proxy",
            Self::Control => "control",
            Self::Backend => "backend",
            Self::Jvm => "jvm",
        }
    }

    /// Classifies a tracing target by the crate that emitted it.
    pub fn of(target: &str) -> Self {
        match target.split("::").next().unwrap_or(target) {
            "chunk_proxy" | "chunk_edge" | "chunk_protocol" => Self::Proxy,
            "chunk_control" => Self::Control,
            "chunk_backend" | "chunk_js" | "chunk_store" => Self::Backend,
            _ => Self::Dev,
        }
    }
}

#[derive(Clone)]
pub(crate) struct Reporter(mpsc::UnboundedSender<Event>);

impl Reporter {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Event>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (Self(sender), receiver)
    }

    fn send(&self, event: Event) {
        let _ = self.0.send(event);
    }

    pub fn running(&self, name: &'static str, detail: impl Display) {
        self.send(Event::Step { name, state: Step::Running(detail.to_string()) });
    }

    pub fn done(&self, name: &'static str, detail: impl Display) {
        self.send(Event::Step { name, state: Step::Done(detail.to_string()) });
    }

    pub fn failed(&self, name: &'static str, error: impl Display) {
        self.send(Event::Step { name, state: Step::Failed(error.to_string()) });
    }

    pub fn log(&self, source: Source, line: String) {
        self.send(Event::Log { source, line });
    }

    pub fn deployments(&self, deployments: Vec<Deployment>) {
        self.send(Event::Deployments(deployments));
    }

    pub fn build_progress(&self) -> progress::Progress {
        let reporter = self.clone();
        progress::Progress::new(move |event| match event {
            progress::Event::Started(phase) => reporter.running(
                phase.name(),
                match phase {
                    progress::Phase::Compile => "Gradle chunkArtifacts",
                    progress::Phase::Release => "Packaging release",
                },
            ),
            progress::Event::Finished(phase, elapsed) => reporter.done(phase.name(), seconds(elapsed)),
            progress::Event::Output(line) => reporter.log(Source::Build, line),
        })
    }
}

pub(crate) fn seconds(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}
