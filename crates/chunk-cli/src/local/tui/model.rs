use std::collections::VecDeque;

use super::super::report::{Deployment, Event, Source, Step};

const RETAINED_LINES: usize = 2000;
const JVM: usize = Source::ALL.len() - 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    Logs,
    Nodes,
}

/// Everything the TUI draws, updated only from session events and keys.
pub(super) struct Model {
    pub steps: Vec<(&'static str, Step)>,
    pub deployments: Vec<Deployment>,
    pub logs: [VecDeque<String>; Source::ALL.len()],
    pub selected: usize,
    /// Lines scrolled up from the newest output of the visible log.
    pub scroll: usize,
    pub focus: Focus,
    /// Row selected in the node table.
    pub node: usize,
    /// Short host ID whose JVM lines the log pane is limited to.
    pub filter: Option<String>,
}

impl Model {
    pub fn new() -> Self {
        Self {
            steps: Vec::new(),
            deployments: Vec::new(),
            logs: Default::default(),
            selected: 0,
            scroll: 0,
            focus: Focus::Logs,
            node: 0,
            filter: None,
        }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Step { name, state } => match self.steps.iter_mut().find(|(step, _)| *step == name) {
                Some((_, current)) => *current = state,
                None => self.steps.push((name, state)),
            },
            Event::Log { source, line } => {
                let index = Source::ALL.iter().position(|candidate| *candidate == source).unwrap_or(0);
                let visible = index == self.selected && self.shows(&line);
                let lines = &mut self.logs[index];
                if lines.len() == RETAINED_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
                if self.scroll > 0 {
                    // Keep the viewed lines in place as new output arrives.
                    self.scroll = (self.scroll + usize::from(visible)).min(self.visible().len());
                }
            }
            Event::Deployments(deployments) => {
                self.deployments = deployments;
                self.node = self.node.min(self.hosts().len().saturating_sub(1));
            }
        }
    }

    pub fn select(&mut self, offset: isize) {
        let count = Source::ALL.len().cast_signed();
        self.selected = (self.selected.cast_signed() + offset).rem_euclid(count).cast_unsigned();
        self.filter = None;
        self.scroll = 0;
    }

    pub fn scroll(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines).min(self.visible().len());
    }

    /// Moves focus between the log pane and the node table; the table only takes focus when it has nodes.
    pub fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            Focus::Logs if !self.hosts().is_empty() => Focus::Nodes,
            _ => Focus::Logs,
        };
    }

    pub fn move_node(&mut self, offset: isize) {
        let count = self.hosts().len();
        if count > 0 {
            self.node = self.node.saturating_add_signed(offset).min(count - 1);
        }
    }

    /// Shows only the selected node's JVM output and returns focus to the log pane.
    pub fn open_node(&mut self) {
        if let Some(host) = self.hosts().get(self.node) {
            self.filter = Some(host[..host.len().min(8)].to_owned());
            self.selected = JVM;
            self.scroll = 0;
            self.focus = Focus::Logs;
        }
    }

    /// Leaves the node table or a node's log; returns false when there is nothing to leave.
    pub fn back(&mut self) -> bool {
        if self.focus == Focus::Nodes {
            self.focus = Focus::Logs;
        } else if self.filter.take().is_some() {
            self.scroll = 0;
        } else {
            return false;
        }
        true
    }

    /// Lines of the selected log, limited to the opened node.
    pub fn visible(&self) -> Vec<&str> {
        self.logs[self.selected].iter().map(String::as_str).filter(|line| self.shows(line)).collect()
    }

    /// Host IDs in node-table order.
    pub fn hosts(&self) -> Vec<&str> {
        self.deployments.iter().flat_map(|deployment| &deployment.nodes).map(|node| node.host_id.as_str()).collect()
    }

    pub fn players(&self) -> u32 {
        self.deployments
            .iter()
            .flat_map(|deployment| &deployment.nodes)
            .filter_map(|node| node.health.as_ref())
            .map(|health| health.players)
            .sum()
    }

    fn shows(&self, line: &str) -> bool {
        self.filter
            .as_ref()
            .is_none_or(|host| line.strip_prefix(host.as_str()).is_some_and(|rest| rest.starts_with(' ')))
    }
}

#[cfg(test)]
mod tests;
