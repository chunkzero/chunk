use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use chunk_proto::sync::v1::{Node, NodePhase, NodeSession};

use super::super::{
    logs,
    report::{Deployment, Event, Source, Step},
};

mod players;

pub(super) use players::{Input, MoveForm};

const RETAINED_LINES: usize = 2000;
pub(super) const STARTUP: [&str; 7] = ["Project", "Compile", "Release", "Java", "Backend", "Control", "Proxy"];

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Log(Source),
    Players,
}

impl Tab {
    pub const ALL: [Self; 7] = [
        Self::Log(Source::Dev),
        Self::Log(Source::Build),
        Self::Log(Source::Proxy),
        Self::Log(Source::Control),
        Self::Log(Source::Backend),
        Self::Log(Source::Jvm),
        Self::Players,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Log(source) => source.name(),
            Self::Players => "players",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Logs,
    Nodes,
}

/// Which JVM log scope is selected in the JVM tab.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum JvmSelection {
    #[default]
    All,
    Node(String),
    Session {
        host: String,
        session: String,
    },
}

pub(super) struct Progress {
    pub name: &'static str,
    pub state: Step,
    pub started: Option<Instant>,
    elapsed: Duration,
}

impl Progress {
    fn pending(name: &'static str, detail: &str) -> Self {
        Self { name, state: Step::Pending(detail.into()), started: None, elapsed: Duration::ZERO }
    }

    pub fn elapsed(&self) -> Duration {
        self.started.map_or(self.elapsed, |started| started.elapsed())
    }

    pub fn detail(&self) -> &str {
        match &self.state {
            Step::Pending(detail) | Step::Running(detail) | Step::Done(detail) | Step::Failed(detail) => detail,
        }
    }

    fn update(&mut self, state: Step) {
        if matches!(state, Step::Running(_)) {
            if self.started.is_none() {
                self.started = Some(Instant::now());
            }
        } else if let Some(started) = self.started.take() {
            self.elapsed = started.elapsed();
        }
        self.state = state;
    }
}

/// Everything the TUI draws, updated only from session events and keys.
pub(super) struct Model {
    pub steps: Vec<Progress>,
    pub deployments: Vec<Deployment>,
    pub logs: [VecDeque<String>; Source::ALL.len()],
    /// Index into `Tab::ALL`.
    pub selected: usize,
    /// Lines scrolled up from the newest output of the visible log.
    pub scroll: usize,
    pub focus: Focus,
    /// Selected node or nested session; `All` selects the combined JVM output.
    pub selection: JvmSelection,
    pub show_details: bool,
    /// Player ID the players tab has selected; the first match when absent or gone.
    pub player: Option<String>,
    pub filter: String,
    pub input: Option<Input>,
    started: Instant,
}

impl Model {
    pub fn new() -> Self {
        let mut steps = [
            ("Project", ""),
            ("Compile", "Waiting for project"),
            ("Release", "Waiting for compilation"),
            ("Java", "Waiting for release"),
            ("Backend", "Waiting for Java"),
            ("Control", "Waiting for backend"),
            ("Proxy", "Waiting for control"),
        ]
        .into_iter()
        .map(|(name, detail)| Progress::pending(name, detail))
        .collect::<Vec<_>>();
        steps[0].update(Step::Running("Inspecting project".into()));
        Self {
            steps,
            deployments: Vec::new(),
            logs: Default::default(),
            selected: Source::Build.index(),
            scroll: 0,
            focus: Focus::Logs,
            selection: JvmSelection::All,
            show_details: false,
            player: None,
            filter: String::new(),
            input: None,
            started: Instant::now(),
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Step { name, state } => self.update_step(name, state),
            Event::Log { source, line } => self.push(source, line),
            Event::Deployments(deployments) => {
                self.deployments = deployments;
                match &self.selection {
                    JvmSelection::All => {}
                    JvmSelection::Node(_) if self.selected_node().is_some() => {}
                    JvmSelection::Session { .. } if self.selected_session().is_some() => {}
                    JvmSelection::Session { host, .. } if self.find_node(host).is_some() => {
                        self.selection = JvmSelection::Node(host.clone());
                        self.scroll = 0;
                    }
                    JvmSelection::Node(_) | JvmSelection::Session { .. } => {
                        self.selection = JvmSelection::All;
                        self.scroll = 0;
                    }
                }
            }
        }
    }

    fn update_step(&mut self, name: &'static str, state: Step) {
        if name == "Reload"
            && let Step::Running(detail) = &state
        {
            for (name, detail) in [("Compile", "Waiting to compile"), ("Release", "Waiting for compilation")] {
                if let Some(step) = self.steps.iter_mut().find(|step| step.name == name) {
                    *step = Progress::pending(name, detail);
                }
            }
            self.push(Source::Build, format!("── Reload · {detail} ──"));
        }
        if let Step::Failed(error) = &state {
            self.push(Source::Dev, format!("{name} failed"));
            for line in error.lines() {
                self.push(Source::Dev, line.to_owned());
            }
            if name == "Reload" {
                for step in &mut self.steps {
                    if matches!(step.name, "Compile" | "Release") && matches!(step.state, Step::Running(_)) {
                        step.update(Step::Failed(error.clone()));
                    }
                }
            }
        }
        let first_ready = name == "Ready" && matches!(state, Step::Done(_)) && !self.ready();
        if let Some(step) = self.steps.iter_mut().find(|step| step.name == name) {
            step.update(state);
        } else {
            let mut step = Progress::pending(name, "");
            step.update(state);
            self.steps.push(step);
        }
        if first_ready && self.tab() == Tab::Log(Source::Build) && self.scroll == 0 {
            self.selected = Source::Jvm.index();
            self.focus = Focus::Nodes;
        }
    }

    pub fn step(&self, name: &str) -> Option<&Progress> {
        self.steps.iter().find(|step| step.name == name)
    }

    pub fn ready(&self) -> bool {
        self.step("Ready").is_some_and(|step| matches!(step.state, Step::Done(_)))
    }

    pub fn tab(&self) -> Tab {
        Tab::ALL[self.selected]
    }

    fn on_jvm(&self) -> bool {
        self.tab() == Tab::Log(Source::Jvm)
    }

    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[usize::try_from((self.started.elapsed().as_millis() / 100) % 10).unwrap_or(0)]
    }

    /// Cycles tabs; only log tabs exist until the session is ready.
    pub fn select(&mut self, offset: isize) {
        let count = if self.ready() { Tab::ALL.len() } else { Source::ALL.len() }.cast_signed();
        self.selected = (self.selected.cast_signed() + offset).rem_euclid(count).cast_unsigned();
        self.focus = if self.ready() && self.on_jvm() { Focus::Nodes } else { Focus::Logs };
        self.scroll = 0;
    }

    pub fn scroll(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines).min(self.visible().len());
    }

    pub fn toggle_focus(&mut self) {
        if self.ready() && self.on_jvm() {
            self.focus = match self.focus {
                Focus::Logs => Focus::Nodes,
                Focus::Nodes => Focus::Logs,
            };
        }
    }

    /// Selecting a node or nested session immediately previews its logs; row zero is all nodes.
    pub fn move_node(&mut self, offset: isize) {
        if !self.on_jvm() {
            return;
        }
        let entries = self.jvm_entries();
        let current = entries.iter().position(|entry| entry == &self.selection).unwrap_or(0);
        let next = current.saturating_add_signed(offset).min(entries.len().saturating_sub(1));
        self.selection = entries[next].clone();
        self.scroll = 0;
    }

    pub fn jvm_entries(&self) -> Vec<JvmSelection> {
        let mut entries = vec![JvmSelection::All];
        for deployment in &self.deployments {
            for (host, node) in &deployment.nodes {
                entries.push(JvmSelection::Node(host.clone()));
                if node.phase() != NodePhase::Stopped {
                    for session in &node.sessions {
                        entries.push(JvmSelection::Session { host: host.clone(), session: session.id.clone() });
                    }
                }
            }
        }
        entries
    }

    pub fn open_node(&mut self) {
        self.focus = Focus::Logs;
    }

    pub fn back(&mut self) {
        if self.ready() && self.on_jvm() {
            self.focus = Focus::Nodes;
        }
    }

    pub fn visible(&self) -> Vec<&str> {
        let Tab::Log(source) = self.tab() else { return Vec::new() };
        self.logs[source.index()].iter().map(String::as_str).filter(|line| self.shows(line)).collect()
    }

    pub fn hosts(&self) -> Vec<&str> {
        self.deployments.iter().flat_map(|deployment| &deployment.nodes).map(|(host, _)| host.as_str()).collect()
    }

    fn find_node(&self, host: &str) -> Option<(&Deployment, &(String, Node))> {
        self.deployments
            .iter()
            .find_map(|deployment| deployment.nodes.iter().find(|(id, _)| id == host).map(|node| (deployment, node)))
    }

    pub fn selected_node(&self) -> Option<(&Deployment, &(String, Node))> {
        let JvmSelection::Node(host) = &self.selection else { return None };
        self.find_node(host)
    }

    pub fn selected_session(&self) -> Option<(&Deployment, &(String, Node), &NodeSession)> {
        let JvmSelection::Session { host, session } = &self.selection else { return None };
        let (deployment, node) = self.find_node(host)?;
        if node.1.phase() == NodePhase::Stopped {
            return None;
        }
        let open = node.1.sessions.iter().find(|candidate| candidate.id == *session)?;
        Some((deployment, node, open))
    }

    pub fn sessions(&self) -> usize {
        self.deployments
            .iter()
            .flat_map(|deployment| &deployment.nodes)
            .filter(|(_, node)| node.phase() != NodePhase::Stopped)
            .map(|(_, node)| node.sessions.len())
            .sum()
    }

    pub fn players(&self) -> u32 {
        self.deployments
            .iter()
            .flat_map(|deployment| &deployment.nodes)
            .filter(|(_, node)| node.phase() != NodePhase::Stopped)
            .filter_map(|(_, node)| node.health.as_ref())
            .map(|health| health.players)
            .sum()
    }

    fn push(&mut self, source: Source, line: String) {
        let index = source.index();
        let visible = self.tab() == Tab::Log(source) && self.shows(&line);
        let lines = &mut self.logs[index];
        if lines.len() == RETAINED_LINES {
            lines.pop_front();
        }
        lines.push_back(line);
        if self.scroll > 0 {
            self.scroll = (self.scroll + usize::from(visible)).min(self.visible().len());
        }
    }

    fn shows(&self, line: &str) -> bool {
        if !self.on_jvm() {
            return true;
        }
        match &self.selection {
            JvmSelection::All => true,
            // Node views keep untagged JVM-wide lines (startup, GC) alongside every session on that host.
            JvmSelection::Node(host) => logs::parse_jvm_line(line).host == &host[..host.len().min(8)],
            // Session views show only lines tagged with that session; untagged JVM lines stay at the node level.
            JvmSelection::Session { host, session } => {
                let parsed = logs::parse_jvm_line(line);
                parsed.host == &host[..host.len().min(8)] && parsed.session == Some(session.as_str())
            }
        }
    }
}

#[cfg(test)]
mod tests;
