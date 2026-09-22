use std::{fmt::Display, time::Duration};

use tokio::sync::mpsc;

/// Progress from a dev session, rendered as plain lines.
pub(crate) enum Event {
    Step { name: &'static str, state: Step },
}

pub(crate) enum Step {
    Running,
    Done(String),
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
}

pub(crate) fn seconds(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}
