use tokio::sync::mpsc;

use super::report::{Event, Step};

/// Prints each step as one line until the session drops its reporters.
pub(super) async fn render(mut events: mpsc::UnboundedReceiver<Event>) {
    while let Some(event) = events.recv().await {
        let _ = match event {
            Event::Step { name, state: Step::Running } => cliclack::log::step(format!("{name}…")),
            Event::Step { name, state: Step::Done(detail) } => cliclack::log::success(format!("{name} · {detail}")),
        };
    }
}
