use chunk_proto::v1::{ClaimPhase, PlayerStatus};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};

use super::{
    super::model::{Input, Model, id_of, name},
    nodes::{count, short},
};

pub(super) fn render(frame: &mut Frame, model: &Model, area: Rect) {
    let [list, detail] = if area.width >= 80 {
        Layout::horizontal([Constraint::Length(30), Constraint::Min(1)]).areas(area)
    } else {
        Layout::vertical([
            Constraint::Length(7.min(area.height / 2).min(area.height.saturating_sub(4))),
            Constraint::Min(1),
        ])
        .areas(area)
    };
    render_list(frame, model, list, area.width >= 80);
    render_detail(frame, model, detail);
}

fn render_list(frame: &mut Frame, model: &Model, area: Rect, sidebar: bool) {
    let roster = model.roster();
    let searching = matches!(model.input, Some(Input::Search));
    let title = if model.filter.is_empty() {
        format!("Players · {}", model.player_count())
    } else {
        format!("Players · {}/{}", roster.len(), model.player_count())
    };
    let block = Block::default()
        .title(title)
        .borders(if sidebar { Borders::RIGHT } else { Borders::BOTTOM })
        .border_style(Style::new().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let search = u16::from(searching || !model.filter.is_empty());
    let [search_area, list] = Layout::vertical([Constraint::Length(search), Constraint::Min(0)]).areas(inner);
    let cursor = if searching { "▏" } else { "" };
    frame.render_widget(
        Line::from(vec![Span::raw("/ ").dark_gray(), Span::raw(format!("{}{cursor}", model.filter))]),
        search_area,
    );

    if roster.is_empty() {
        let empty = if model.player_count() == 0 { "No players online" } else { "No matches" };
        frame.render_widget(Line::from(format!("  {empty}")).dark_gray(), list);
        return;
    }
    let items = roster.iter().map(|(_, player)| {
        let (label, color) = state(player);
        let demand = player.demand.clone().unwrap_or_default();
        ListItem::new(vec![
            Line::from(vec![
                Span::styled("● ", Style::new().fg(color)),
                Span::raw(name(player)).bold(),
                Span::raw(format!(" · {label}")).dark_gray(),
            ]),
            Line::from(format!("  {}:{}", demand.session_type, demand.key)).dark_gray(),
        ])
    });
    let mut state = ListState::default().with_selected(model.selected_player().map(|(index, ..)| index));
    let list_widget = List::new(items)
        .highlight_symbol(if model.input.is_none() { "> " } else { "  " })
        // One background for the row; reversing would invert each styled span separately.
        .highlight_style(Style::new().bg(Color::Indexed(237)));
    frame.render_stateful_widget(list_widget, list, &mut state);
}

fn render_detail(frame: &mut Frame, model: &Model, area: Rect) {
    let [header, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
    let [heading, metadata] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(header);
    let metadata_block = Block::default().borders(Borders::BOTTOM).border_style(Style::new().dark_gray());
    let Some((_, deployment, player)) = model.selected_player() else {
        frame.render_widget(Line::from("No player selected").bold(), heading);
        frame.render_widget(
            Paragraph::new(count(model.player_count(), "player")).dark_gray().block(metadata_block),
            metadata,
        );
        render_result(frame, model, Vec::new(), body);
        return;
    };
    let (label, color) = state(player);
    frame.render_widget(
        Line::from(vec![Span::raw(format!("{}  ", name(player))).bold(), Span::styled(label, Style::new().fg(color))]),
        heading,
    );
    let mut identity = id_of(player).to_owned();
    if player.since_ms > 0 {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        let since = u64::try_from(now).unwrap_or(u64::MAX).saturating_sub(player.since_ms);
        identity = format!("{identity} · here {}", elapsed(since));
    }
    frame.render_widget(Paragraph::new(identity).dark_gray().block(metadata_block), metadata);

    let demand = player.demand.clone().unwrap_or_default();
    let mut lines = [
        ("Session", format!("{} · {}", demand.session_type, demand.key)),
        ("Node", format!("{} · {}", player.app_id, short(&player.host_id))),
        ("Release", format!("{} · {}", short(&deployment.id), deployment.state)),
        ("Profile", demand.machine_profile),
    ]
    .into_iter()
    .map(|(field, value)| Line::from(vec![Span::raw(format!("{field:<10}")).dark_gray(), Span::raw(value)]))
    .collect::<Vec<_>>();
    lines.push(Line::default());
    match &model.input {
        Some(Input::Move(form)) => {
            lines.push(Line::from(format!("Move {}", form.name)).bold());
            for (index, (session_type, profile)) in form.session_types.iter().enumerate() {
                let chosen = index == form.choice;
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}{session_type}", if chosen { "> " } else { "  " }),
                        if chosen { Style::new().fg(Color::Cyan).bold() } else { Style::new() },
                    ),
                    Span::raw(format!(" · {profile}")).dark_gray(),
                ]));
            }
            lines.push(Line::from(vec![Span::raw("Key  ").dark_gray(), Span::raw(format!("{}▏", form.key))]));
        }
        _ => lines.push(Line::from("m move · / search").dark_gray()),
    }
    render_result(frame, model, lines, body);
}

/// Appends the latest move outcome below the detail lines.
fn render_result<'a>(frame: &mut Frame, model: &'a Model, mut lines: Vec<Line<'a>>, area: Rect) {
    if let Some(step) = model.step("Move") {
        let (mark, color) = super::mark(model, &step.state);
        let detail = step.detail().lines().next().unwrap_or_default();
        lines.push(Line::default());
        lines.push(Line::from(vec![Span::styled(format!("{mark} Move  "), Style::new().fg(color)), Span::raw(detail)]));
    }
    frame.render_widget(Paragraph::new(lines), area);
}

fn state(player: &PlayerStatus) -> (&'static str, Color) {
    if player.moving {
        return ("moving", Color::Yellow);
    }
    match ClaimPhase::try_from(player.phase).unwrap_or(ClaimPhase::Unspecified) {
        ClaimPhase::Arrived => ("online", Color::Green),
        ClaimPhase::Reserved | ClaimPhase::Activating | ClaimPhase::Attached => ("joining", Color::Yellow),
        ClaimPhase::Withdrawing => ("leaving", Color::Yellow),
        ClaimPhase::Released | ClaimPhase::Unspecified => ("offline", Color::DarkGray),
    }
}

fn elapsed(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m{:02}s", seconds / 60, seconds % 60),
        _ => format!("{}h{:02}m", seconds / 3600, seconds / 60 % 60),
    }
}
