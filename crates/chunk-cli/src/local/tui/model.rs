use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use chunk_proto::v1::NodeStatus;

use super::super::report::{Deployment, Event, Source, Step};

const RETAINED_LINES: usize = 2000;
pub(super) const STARTUP: [&str; 7] = ["Project", "Compile", "Release", "Java", "Backend", "Control", "Proxy"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Logs,
    Nodes,
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
    pub selected: usize,
    /// Lines scrolled up from the newest output of the visible log.
    pub scroll: usize,
    pub focus: Focus,
    /// Full host identity; None selects the combined JVM output.
    pub node: Option<String>,
    pub show_details: bool,
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
            node: None,
            show_details: false,
            started: Instant::now(),
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Step { name, state } => self.update_step(name, state),
            Event::Log { source, line } => self.push(source, line),
            Event::Deployments(deployments) => {
                self.deployments = deployments;
                if self.node.is_some() && self.selected_node().is_none() {
                    self.node = None;
                    self.scroll = 0;
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
        if first_ready && self.source() == Source::Build && self.scroll == 0 {
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

    pub fn source(&self) -> Source {
        Source::ALL[self.selected]
    }

    pub fn spinner(&self) -> &'static str {
        const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        FRAMES[usize::try_from((self.started.elapsed().as_millis() / 100) % 10).unwrap_or(0)]
    }

    pub fn select(&mut self, offset: isize) {
        let count = Source::ALL.len().cast_signed();
        self.selected = (self.selected.cast_signed() + offset).rem_euclid(count).cast_unsigned();
        self.focus = if self.ready() && self.source() == Source::Jvm { Focus::Nodes } else { Focus::Logs };
        self.scroll = 0;
    }

    pub fn scroll(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines).min(self.visible().len());
    }

    pub fn toggle_focus(&mut self) {
        if self.ready() && self.source() == Source::Jvm {
            self.focus = match self.focus {
                Focus::Logs => Focus::Nodes,
                Focus::Nodes => Focus::Logs,
            };
        }
    }

    /// Selecting a node immediately previews its logs; row zero is all nodes.
    pub fn move_node(&mut self, offset: isize) {
        if self.source() != Source::Jvm {
            return;
        }
        let hosts = self.hosts();
        let selected = self.node.as_deref().and_then(|node| hosts.iter().position(|host| *host == node));
        let row = selected.map_or(0, |index| index + 1).saturating_add_signed(offset).min(hosts.len());
        self.node = row.checked_sub(1).map(|index| hosts[index].to_owned());
        self.scroll = 0;
    }

    pub fn open_node(&mut self) {
        self.focus = Focus::Logs;
    }

    pub fn back(&mut self) {
        if self.ready() && self.source() == Source::Jvm {
            self.focus = Focus::Nodes;
        }
    }

    pub fn visible(&self) -> Vec<&str> {
        self.logs[self.selected].iter().map(String::as_str).filter(|line| self.shows(line)).collect()
    }

    pub fn hosts(&self) -> Vec<&str> {
        self.deployments.iter().flat_map(|deployment| &deployment.nodes).map(|node| node.host_id.as_str()).collect()
    }

    pub fn selected_node(&self) -> Option<(&Deployment, &NodeStatus)> {
        let host = self.node.as_deref()?;
        self.deployments.iter().find_map(|deployment| {
            deployment.nodes.iter().find(|node| node.host_id == host).map(|node| (deployment, node))
        })
    }

    pub fn players(&self) -> u32 {
        self.deployments
            .iter()
            .flat_map(|deployment| &deployment.nodes)
            .filter_map(|node| node.health.as_ref())
            .map(|health| health.players)
            .sum()
    }

    fn push(&mut self, source: Source, line: String) {
        let index = source.index();
        let visible = index == self.selected && self.shows(&line);
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
        self.source() != Source::Jvm
            || self.node.as_ref().is_none_or(|host| {
                line.strip_prefix(&host[..host.len().min(8)]).is_some_and(|rest| rest.starts_with(' '))
            })
    }
}

#[cfg(test)]
mod tests;
