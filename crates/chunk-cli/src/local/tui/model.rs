use std::collections::VecDeque;

use super::super::report::{Deployment, Event, Source, Step};

const RETAINED_LINES: usize = 2000;

/// Everything the TUI draws, updated only from session events and keys.
pub(super) struct Model {
    pub steps: Vec<(&'static str, Step)>,
    pub deployments: Vec<Deployment>,
    pub logs: [VecDeque<String>; Source::ALL.len()],
    pub selected: usize,
    /// Lines scrolled up from the newest output of the selected log.
    pub scroll: usize,
}

impl Model {
    pub fn new() -> Self {
        Self { steps: Vec::new(), deployments: Vec::new(), logs: Default::default(), selected: 0, scroll: 0 }
    }

    pub fn apply(&mut self, event: Event) {
        match event {
            Event::Step { name, state } => match self.steps.iter_mut().find(|(step, _)| *step == name) {
                Some((_, current)) => *current = state,
                None => self.steps.push((name, state)),
            },
            Event::Log { source, line } => {
                let index = Source::ALL.iter().position(|candidate| *candidate == source).unwrap_or(0);
                let lines = &mut self.logs[index];
                if lines.len() == RETAINED_LINES {
                    lines.pop_front();
                }
                lines.push_back(line);
                if index == self.selected && self.scroll > 0 {
                    self.scroll = (self.scroll + 1).min(lines.len());
                }
            }
            Event::Deployments(deployments) => self.deployments = deployments,
        }
    }

    pub fn select(&mut self, offset: isize) {
        let count = Source::ALL.len().cast_signed();
        self.selected = (self.selected.cast_signed() + offset).rem_euclid(count).cast_unsigned();
        self.scroll = 0;
    }

    pub fn scroll(&mut self, lines: isize) {
        self.scroll = self.scroll.saturating_add_signed(lines).min(self.logs[self.selected].len());
    }

    pub fn players(&self) -> u32 {
        self.deployments
            .iter()
            .flat_map(|deployment| &deployment.nodes)
            .filter_map(|node| node.health.as_ref())
            .map(|health| health.players)
            .sum()
    }
}
