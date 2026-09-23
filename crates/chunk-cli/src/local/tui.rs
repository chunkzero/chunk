use std::{io, net::SocketAddr, time::Duration};

use ratatui::crossterm::event::{self, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Command, report::Event};
use model::Focus;

mod model;
mod view;

pub(super) struct Info {
    pub title: String,
    pub address: SocketAddr,
    pub watching: bool,
}

/// Draws the session until `finished`; quitting cancels `stop` so the session shuts down first.
pub(super) fn run(
    mut events: mpsc::UnboundedReceiver<Event>,
    commands: &mpsc::UnboundedSender<Command>,
    info: &Info,
    stop: &CancellationToken,
    finished: &CancellationToken,
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut model = model::Model::new();
    let result = loop {
        for _ in 0..512 {
            match events.try_recv() {
                Ok(event) => model.apply(event),
                Err(_) => break,
            }
        }
        if finished.is_cancelled() {
            break Ok(());
        }
        if let Err(error) = terminal.draw(|frame| view::render(frame, &model, info)) {
            break Err(error);
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(false) => {}
            Ok(true) => match event::read() {
                Ok(event::Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => stop.cancel(),
                    KeyCode::Esc => model.back(),
                    KeyCode::Char('b') => model.show_details = !model.show_details,
                    KeyCode::Char('r') => {
                        let _ = commands.send(Command::Restart);
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => stop.cancel(),
                    KeyCode::Tab | KeyCode::BackTab => model.toggle_focus(),
                    KeyCode::Left => model.select(-1),
                    KeyCode::Right => model.select(1),
                    KeyCode::Up if model.focus == Focus::Nodes => model.move_node(-1),
                    KeyCode::Down if model.focus == Focus::Nodes => model.move_node(1),
                    KeyCode::Enter if model.focus == Focus::Nodes => model.open_node(),
                    KeyCode::Up => model.scroll(1),
                    KeyCode::Down => model.scroll(-1),
                    KeyCode::PageUp => model.scroll(20),
                    KeyCode::PageDown => model.scroll(-20),
                    KeyCode::End => model.scroll = 0,
                    _ => {}
                },
                Ok(_) => {}
                Err(error) => break Err(error),
            },
            Err(error) => break Err(error),
        }
    };
    ratatui::restore();
    if result.is_err() {
        // Without the UI there are no controls, so shut the session down instead of running headless.
        stop.cancel();
    }
    result
}
