use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::report::{Event, Step};

/// Prints each step as one line until `finished`, then flushes the remaining events.
pub(super) async fn render(mut events: mpsc::UnboundedReceiver<Event>, finished: CancellationToken) {
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            () = finished.cancelled() => events.try_recv().ok(),
        };
        let Some(event) = event else { return };
        let _ = match event {
            Event::Step { name, state: Step::Running } => cliclack::log::step(format!("{name}…")),
            Event::Step { name, state: Step::Done(detail) } => cliclack::log::success(format!("{name} · {detail}")),
            Event::Log { source, line } => cliclack::log::remark(format!("[{}] {line}", source.name())),
            Event::Deployments(_) => Ok(()),
        };
    }
}
