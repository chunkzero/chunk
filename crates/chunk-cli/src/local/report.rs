use std::{fmt::Display, time::Duration};

use chunk_proto::v1::NodeStatus;
use tokio::sync::mpsc;

/// Progress and diagnostics from a dev session, rendered as plain lines or by the TUI.
pub(crate) enum Event {
    Step { name: &'static str, state: Step },
    Log { source: Source, line: String },
    Deployments(Vec<Deployment>),
}

pub(crate) enum Step {
    Running,
    Done(String),
}

/// One running release and the nodes its control authority reports.
pub(crate) struct Deployment {
    pub id: String,
    pub state: String,
    pub nodes: Vec<NodeStatus>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Dev,
    Proxy,
    Control,
    Backend,
    Jvm,
}

impl Source {
    pub const ALL: [Self; 5] = [Self::Dev, Self::Proxy, Self::Control, Self::Backend, Self::Jvm];

    pub fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
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

    pub fn running(&self, name: &'static str) {
        self.send(Event::Step { name, state: Step::Running });
    }

    pub fn done(&self, name: &'static str, detail: impl Display) {
        self.send(Event::Step { name, state: Step::Done(detail.to_string()) });
    }

    pub fn log(&self, source: Source, line: String) {
        self.send(Event::Log { source, line });
    }

    pub fn deployments(&self, deployments: Vec<Deployment>) {
        self.send(Event::Deployments(deployments));
    }
}

pub(crate) fn seconds(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}
