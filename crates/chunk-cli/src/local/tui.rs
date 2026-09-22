use std::{io, time::Duration};

use ratatui::crossterm::event::{self, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::report::Event;

mod model;
mod view;

/// Draws the session until `finished`; quitting cancels `stop` so the session shuts down first.
pub(super) fn run(
    mut events: mpsc::UnboundedReceiver<Event>,
    title: &str,
    stop: &CancellationToken,
    finished: &CancellationToken,
) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut model = model::Model::new();
    let result = loop {
        while let Ok(event) = events.try_recv() {
            model.apply(event);
        }
        if finished.is_cancelled() {
            break Ok(());
        }
        if let Err(error) = terminal.draw(|frame| view::render(frame, &model, title)) {
            break Err(error);
        }
        match event::poll(Duration::from_millis(100)) {
            Ok(false) => {}
            Ok(true) => match event::read() {
                Ok(event::Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => stop.cancel(),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => stop.cancel(),
                    KeyCode::Left | KeyCode::BackTab => model.select(-1),
                    KeyCode::Right | KeyCode::Tab => model.select(1),
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
    result
}
