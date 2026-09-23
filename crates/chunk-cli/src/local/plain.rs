use std::io::BufRead;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    Command,
    report::{Event, Step},
};

/// Reads `r` lines from stdin as restart requests on a detached thread that never delays exit.
pub(super) fn read_commands(commands: mpsc::UnboundedSender<Command>) {
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { return };
            if line.trim() == "r" && commands.send(Command::Restart).is_err() {
                return;
            }
        }
    });
}

/// Prints each step as one line until `finished`, then flushes the remaining events.
pub(super) async fn render(mut events: mpsc::UnboundedReceiver<Event>, finished: CancellationToken) {
    loop {
        let event = tokio::select! {
            event = events.recv() => event,
            () = finished.cancelled() => events.try_recv().ok(),
        };
        let Some(event) = event else { return };
        let _ = match event {
            Event::Step { name, state: Step::Running(detail) } => {
                cliclack::log::step(format!("{name}… {detail}").trim_end())
            }
            Event::Step { name, state: Step::Done(detail) } => cliclack::log::success(format!("{name} · {detail}")),
            Event::Step { name, state: Step::Failed(error) } => cliclack::log::error(format!("{name} failed\n{error}")),
            Event::Log { source, line } => cliclack::log::remark(format!("[{}] {line}", source.name())),
            Event::Deployments(_) => Ok(()),
        };
    }
}
